use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde::{Deserialize, Serialize};
use sha1::{Digest, Sha1};
use thiserror::Error;
use walkdir::WalkDir;

use crate::config::{Config, ConfigError, DownloadSource, JavaMemoryConfig};
use crate::curseforge::client::CurseForgeClient;
use crate::curseforge::model::CfFile;
use crate::curseforge::resolver::CurseForgeApi;
use crate::dependency::DependencyType;
use crate::project::build::{
    BuildError, RuntimeResolvedEntry, RuntimeResolvedLists, resolve_project_for_runtime_with_api,
};

const GENERATED_DIRS: &[&str] = &[
    "mods",
    "resourcepacks",
    "shaderpacks",
    "libraries",
    "versions",
    "assets",
];
const SYNC_EXCLUDES: &[&str] = &[
    "mods",
    "resourcepacks",
    "shaderpacks",
    "libraries",
    "versions",
    "assets",
    ".mcmpu",
];
const OFFLINE_ACCESS_TOKEN: &str = "0";
const RUN_SOURCES_FILE: &str = ".mcmpu-run-sources.json";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunSide {
    Client,
    Server,
}

impl RunSide {
    pub fn dir_name(self) -> &'static str {
        match self {
            Self::Client => "client",
            Self::Server => "server",
        }
    }

    pub fn pending_sync_file_name(self) -> &'static str {
        match self {
            Self::Client => "pending-sync-client.json",
            Self::Server => "pending-sync-server.json",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RunFile {
    pub path: String,
    pub url: String,
    pub cache_path: PathBuf,
    pub sha1: String,
    pub size: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunLaunchPlan {
    pub side: RunSide,
    pub run_dir: PathBuf,
    pub java_args: Vec<String>,
    pub player_name: String,
    pub offline_uuid: String,
    pub files: Vec<RunFile>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchCommand {
    pub side: RunSide,
    pub working_dir: PathBuf,
    pub executable: PathBuf,
    pub args: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunProgress {
    ResolvingProject {
        side: RunSide,
        project_dir: PathBuf,
    },
    RebuildingRunDirectory {
        side: RunSide,
        run_dir: PathBuf,
    },
    CopyingOverrides {
        side: RunSide,
    },
    EnsuringSyncConfig {
        side: RunSide,
        path: PathBuf,
    },
    RunManifestWritten {
        file_count: usize,
    },
    DownloadingFiles {
        total: usize,
    },
    FileReady {
        completed: usize,
        total: usize,
        path: String,
    },
    ResolvingJava {
        major: u16,
    },
    Launching {
        command: String,
    },
    PendingSyncWritten {
        path: PathBuf,
        entries: usize,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CurseMavenFile {
    pub slug: String,
    pub project_id: u64,
    pub file_id: u64,
    pub original_file_name: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SyncTarget {
    Common,
    Client,
    Server,
}

impl SyncTarget {
    pub fn dir_name(self) -> &'static str {
        match self {
            Self::Common => "common",
            Self::Client => "client",
            Self::Server => "server",
        }
    }

    fn from_run_side(side: RunSide) -> Self {
        match side {
            RunSide::Client => Self::Client,
            RunSide::Server => Self::Server,
        }
    }
}

impl std::fmt::Display for SyncTarget {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.dir_name())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunSourceKind {
    OverridesCommon,
    OverridesClient,
    OverridesServer,
    DebugCommon,
    DebugClient,
    DebugServer,
}

impl RunSourceKind {
    fn target(self) -> Option<SyncTarget> {
        match self {
            Self::OverridesCommon => Some(SyncTarget::Common),
            Self::OverridesClient => Some(SyncTarget::Client),
            Self::OverridesServer => Some(SyncTarget::Server),
            Self::DebugCommon | Self::DebugClient | Self::DebugServer => None,
        }
    }

    fn is_debug(self) -> bool {
        matches!(
            self,
            Self::DebugCommon | Self::DebugClient | Self::DebugServer
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunSourceEntry {
    pub path: String,
    pub source: RunSourceKind,
    pub target: Option<SyncTarget>,
    pub source_path: String,
    pub sha1: String,
    pub size: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunSourceManifest {
    pub side: RunSide,
    pub entries: Vec<RunSourceEntry>,
}

impl RunSourceManifest {
    pub fn from_path(path: impl AsRef<Path>) -> Result<Self, RunError> {
        let path = path.as_ref();
        let text = fs::read_to_string(path).map_err(|source| RunError::ReadFile {
            path: path.display().to_string(),
            source,
        })?;
        serde_json::from_str(&text).map_err(|source| RunError::ParseJson {
            path: path.display().to_string(),
            source,
        })
    }

    pub fn entry(&self, path: &str) -> Option<&RunSourceEntry> {
        self.entries.iter().find(|entry| entry.path == path)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PendingSyncChange {
    Modified,
    Deleted,
    Added,
}

impl std::fmt::Display for PendingSyncChange {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Modified => "modified",
            Self::Deleted => "deleted",
            Self::Added => "added",
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingSyncEntry {
    pub path: String,
    pub change: PendingSyncChange,
    pub source: Option<RunSourceKind>,
    pub target: SyncTarget,
    pub apply: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingSyncManifest {
    pub side: RunSide,
    pub run_dir: PathBuf,
    pub path: PathBuf,
    pub ignored_debug: usize,
    pub ignored_generated: usize,
    pub entries: Vec<PendingSyncEntry>,
}

impl PendingSyncManifest {
    pub fn from_path(path: impl AsRef<Path>) -> Result<Self, RunError> {
        let path = path.as_ref();
        let text = fs::read_to_string(path).map_err(|source| RunError::ReadFile {
            path: path.display().to_string(),
            source,
        })?;
        serde_json::from_str(&text).map_err(|source| RunError::ParseJson {
            path: path.display().to_string(),
            source,
        })
    }

    pub fn write(&self) -> Result<(), RunError> {
        if let Some(parent) = self.path.parent() {
            create_dir(parent)?;
        }
        let text = serde_json::to_string_pretty(self)?;
        fs::write(&self.path, text).map_err(|source| RunError::WriteFile {
            path: self.path.display().to_string(),
            source,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SyncConfig {
    pub include_paths: Vec<String>,
}

pub trait DownloadBytes {
    fn download_bytes(&self, url: &str) -> Result<Vec<u8>, RunError>;
}

pub struct HttpDownloader {
    client: reqwest::blocking::Client,
}

pub trait JavaLocator {
    fn path_java(&self, major: u16) -> Option<PathBuf>;
    fn common_java(&self, major: u16) -> Option<PathBuf>;
}

#[derive(Debug, Default)]
pub struct SystemJavaLocator;

pub trait ProcessRunner {
    fn run(&self, command: &LaunchCommand) -> Result<(), RunError>;
}

#[derive(Debug, Default)]
pub struct SystemProcessRunner;

#[derive(Debug, Error)]
pub enum RunError {
    #[error(transparent)]
    Build(#[from] BuildError),
    #[error(transparent)]
    Config(#[from] ConfigError),
    #[error("dependency is missing project_id or file_id for CurseMaven")]
    MissingCurseMavenCoordinate,
    #[error("CurseForge file {project_id}/{file_id} has no SHA1 hash")]
    MissingSha1 { project_id: u64, file_id: u64 },
    #[error("failed to create HTTP client: {0}")]
    HttpClient(#[from] reqwest::Error),
    #[error("failed to download '{url}': {source}")]
    Download {
        url: String,
        #[source]
        source: reqwest::Error,
    },
    #[error("downloaded file '{path}' has size {actual}, expected {expected}")]
    SizeMismatch {
        path: String,
        expected: u64,
        actual: u64,
    },
    #[error("downloaded file '{path}' has SHA1 {actual}, expected {expected}")]
    Sha1Mismatch {
        path: String,
        expected: String,
        actual: String,
    },
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
    #[error("failed to remove directory '{path}': {source}")]
    RemoveDir {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to remove file '{path}': {source}")]
    RemoveFile {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to copy '{from}' to '{to}': {source}")]
    CopyFile {
        from: String,
        to: String,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to write file '{path}': {source}")]
    WriteFile {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to read sync config '{path}': {source}")]
    ReadSyncConfig {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to parse sync config '{path}': {source}")]
    ParseSyncConfig {
        path: String,
        #[source]
        source: toml::de::Error,
    },
    #[error("failed to parse JSON '{path}': {source}")]
    ParseJson {
        path: String,
        #[source]
        source: serde_json::Error,
    },
    #[error("failed to serialize sync config: {0}")]
    SerializeSyncConfig(#[from] toml::ser::Error),
    #[error("failed to serialize JSON: {0}")]
    SerializeJson(#[from] serde_json::Error),
    #[error("failed to walk directory '{path}': {source}")]
    WalkDir {
        path: String,
        #[source]
        source: walkdir::Error,
    },
    #[error(
        "Java major {major} executable was not found. Configure [testRun].java_path or install Java version {major}."
    )]
    JavaNotFound { major: u16 },
    #[error("failed to start run process '{command}': {source}")]
    ProcessStart {
        command: String,
        #[source]
        source: std::io::Error,
    },
    #[error("run process exited with status {status}")]
    ProcessExit { status: std::process::ExitStatus },
}

impl HttpDownloader {
    pub fn new(proxy: Option<&str>) -> Result<Self, RunError> {
        let mut builder = reqwest::blocking::Client::builder();
        if let Some(proxy) = proxy {
            builder = builder.proxy(reqwest::Proxy::all(proxy)?);
        }
        Ok(Self {
            client: builder.build()?,
        })
    }
}

impl DownloadBytes for HttpDownloader {
    fn download_bytes(&self, url: &str) -> Result<Vec<u8>, RunError> {
        let response = self
            .client
            .get(url)
            .send()
            .and_then(reqwest::blocking::Response::error_for_status)
            .map_err(|source| RunError::Download {
                url: url.to_string(),
                source,
            })?;
        response
            .bytes()
            .map(|bytes| bytes.to_vec())
            .map_err(|source| RunError::Download {
                url: url.to_string(),
                source,
            })
    }
}

impl JavaLocator for SystemJavaLocator {
    fn path_java(&self, _major: u16) -> Option<PathBuf> {
        find_on_path(java_executable_name())
    }

    fn common_java(&self, major: u16) -> Option<PathBuf> {
        common_java_candidates(major)
            .into_iter()
            .find(|candidate| candidate.is_file())
    }
}

impl ProcessRunner for SystemProcessRunner {
    fn run(&self, command: &LaunchCommand) -> Result<(), RunError> {
        let status = Command::new(&command.executable)
            .args(&command.args)
            .current_dir(&command.working_dir)
            .status()
            .map_err(|source| RunError::ProcessStart {
                command: format!(
                    "{} {}",
                    command.executable.display(),
                    command.args.join(" ")
                ),
                source,
            })?;
        if status.success() {
            Ok(())
        } else {
            Err(RunError::ProcessExit { status })
        }
    }
}

impl CurseMavenFile {
    pub fn new(slug: &str, file: &CfFile) -> Result<Self, RunError> {
        Ok(Self {
            slug: slug.to_string(),
            project_id: file.mod_id,
            file_id: file.id,
            original_file_name: file.file_name.clone(),
        })
    }
}

impl SyncConfig {
    pub fn from_path(path: impl AsRef<Path>) -> Result<Self, RunError> {
        let path = path.as_ref();
        let text = fs::read_to_string(path).map_err(|source| RunError::ReadSyncConfig {
            path: path.display().to_string(),
            source,
        })?;
        toml::from_str(&text).map_err(|source| RunError::ParseSyncConfig {
            path: path.display().to_string(),
            source,
        })
    }
}

pub fn prepare_run_directory_with_api<A: CurseForgeApi + Sync>(
    project_dir: impl AsRef<Path>,
    api: &A,
    side: RunSide,
) -> Result<RunLaunchPlan, RunError> {
    prepare_run_directory_with_api_and_progress(project_dir, api, side, &mut |_| {})
}

pub fn prepare_run_directory_with_api_and_progress<A, F>(
    project_dir: impl AsRef<Path>,
    api: &A,
    side: RunSide,
    progress: &mut F,
) -> Result<RunLaunchPlan, RunError>
where
    A: CurseForgeApi + Sync,
    F: FnMut(&RunProgress),
{
    let project_dir = project_dir.as_ref();
    progress(&RunProgress::ResolvingProject {
        side,
        project_dir: project_dir.to_path_buf(),
    });
    let (config, resolved) = resolve_project_for_runtime_with_api(project_dir, api)?;
    prepare_run_directory_with_progress(project_dir, &config, &resolved, side, progress)
}

pub fn run_project_with_api_key(
    project_dir: impl AsRef<Path>,
    api_key: String,
    proxy: Option<&str>,
    side: RunSide,
) -> Result<RunLaunchPlan, RunError> {
    run_project_with_api_key_and_progress(project_dir, api_key, proxy, side, &mut |_| {})
}

pub fn run_project_with_api_key_and_progress<F>(
    project_dir: impl AsRef<Path>,
    api_key: String,
    proxy: Option<&str>,
    side: RunSide,
    progress: &mut F,
) -> Result<RunLaunchPlan, RunError>
where
    F: FnMut(&RunProgress),
{
    let project_dir = project_dir.as_ref();
    let api = CurseForgeClient::with_proxy(api_key, proxy).map_err(BuildError::from)?;
    let plan = prepare_run_directory_with_api_and_progress(project_dir, &api, side, progress)?;
    let downloader = HttpDownloader::new(proxy)?;
    verify_or_download_run_files_with_progress(&plan.run_dir, &plan.files, &downloader, progress)?;
    let config = Config::from_path(project_dir.join("config.toml"))?;
    progress(&RunProgress::ResolvingJava {
        major: config.test_run.java_version_major,
    });
    let java = resolve_java_executable(project_dir, &config.test_run, &SystemJavaLocator)?;
    let command = plan_launch_command(&plan, &java)?;
    progress(&RunProgress::Launching {
        command: format!(
            "{} {}",
            command.executable.display(),
            command.args.join(" ")
        ),
    });
    let pending_path = run_launch_command(project_dir, &command, &SystemProcessRunner)?;
    let entries = PendingSyncManifest::from_path(&pending_path)
        .map(|manifest| manifest.entries.len())
        .unwrap_or_default();
    progress(&RunProgress::PendingSyncWritten {
        path: pending_path,
        entries,
    });
    Ok(plan)
}

pub fn prepare_run_directory(
    project_dir: &Path,
    config: &Config,
    resolved: &RuntimeResolvedLists,
    side: RunSide,
) -> Result<RunLaunchPlan, RunError> {
    prepare_run_directory_with_progress(project_dir, config, resolved, side, &mut |_| {})
}

fn prepare_run_directory_with_progress(
    project_dir: &Path,
    config: &Config,
    resolved: &RuntimeResolvedLists,
    side: RunSide,
    progress: &mut impl FnMut(&RunProgress),
) -> Result<RunLaunchPlan, RunError> {
    let run_dir = project_dir.join("run").join(side.dir_name());
    create_dir(&run_dir)?;
    progress(&RunProgress::RebuildingRunDirectory {
        side,
        run_dir: run_dir.clone(),
    });
    rebuild_generated_dirs(&run_dir)?;
    progress(&RunProgress::CopyingOverrides { side });
    let sources = copy_overrides(project_dir, &run_dir, side)?;
    progress(&RunProgress::EnsuringSyncConfig {
        side,
        path: run_dir.join(".mcmpu-sync.toml"),
    });
    ensure_sync_config(project_dir, &run_dir, side)?;
    write_source_manifest(&run_dir, side, sources)?;
    let files = runtime_files(project_dir, resolved, side)?;
    write_run_manifest(&run_dir, &files)?;
    progress(&RunProgress::RunManifestWritten {
        file_count: files.len(),
    });
    Ok(RunLaunchPlan {
        side,
        run_dir,
        java_args: java_args_for_side(config, side),
        player_name: config.test_run.player_name.clone(),
        offline_uuid: offline_uuid_for_player(&config.test_run.player_name),
        files,
    })
}

pub fn verify_or_download_run_files(
    run_dir: &Path,
    files: &[RunFile],
    downloader: &impl DownloadBytes,
) -> Result<(), RunError> {
    verify_or_download_run_files_with_progress(run_dir, files, downloader, &mut |_| {})
}

pub fn verify_or_download_run_files_with_progress(
    run_dir: &Path,
    files: &[RunFile],
    downloader: &impl DownloadBytes,
    progress: &mut impl FnMut(&RunProgress),
) -> Result<(), RunError> {
    progress(&RunProgress::DownloadingFiles { total: files.len() });
    for (index, file) in files.iter().enumerate() {
        if !is_valid_file(&file.cache_path, file.size, &file.sha1)? {
            if let Some(parent) = file.cache_path.parent() {
                create_dir(parent)?;
            }
            let bytes = downloader.download_bytes(&file.url)?;
            verify_bytes(&file.cache_path, &bytes, file.size, &file.sha1)?;
            fs::write(&file.cache_path, bytes).map_err(|source| RunError::WriteFile {
                path: file.cache_path.display().to_string(),
                source,
            })?;
        }

        let target = run_dir.join(&file.path);
        if let Some(parent) = target.parent() {
            create_dir(parent)?;
        }
        fs::copy(&file.cache_path, &target).map_err(|source| RunError::CopyFile {
            from: file.cache_path.display().to_string(),
            to: target.display().to_string(),
            source,
        })?;
        progress(&RunProgress::FileReady {
            completed: index + 1,
            total: files.len(),
            path: file.path.clone(),
        });
    }
    Ok(())
}

pub fn scan_pending_sync(
    project_dir: &Path,
    run_dir: &Path,
    side: RunSide,
) -> Result<PendingSyncManifest, RunError> {
    let sync = SyncConfig::from_path(run_dir.join(".mcmpu-sync.toml"))?;
    let source_manifest = RunSourceManifest::from_path(run_dir.join(RUN_SOURCES_FILE))?;
    let source_by_path = source_manifest
        .entries
        .iter()
        .map(|entry| (entry.path.clone(), entry))
        .collect::<BTreeMap<_, _>>();
    let mut entries = Vec::new();
    let mut seen = BTreeSet::new();
    let mut ignored_debug = 0_usize;
    let mut ignored_generated = 0_usize;

    for source in &source_manifest.entries {
        if source.source.is_debug() {
            let run_path = run_dir.join(&source.path);
            if run_path.exists() {
                let current = file_fingerprint(&run_path)?;
                if current.sha1 != source.sha1 || current.size != source.size {
                    ignored_debug += 1;
                }
            }
            continue;
        }
        let Some(target) = source.target else {
            continue;
        };
        seen.insert(source.path.clone());
        let run_path = run_dir.join(&source.path);
        if !run_path.exists() {
            entries.push(PendingSyncEntry {
                path: source.path.clone(),
                change: PendingSyncChange::Deleted,
                source: Some(source.source),
                target,
                apply: false,
            });
            continue;
        }
        let current = file_fingerprint(&run_path)?;
        if current.sha1 != source.sha1 || current.size != source.size {
            entries.push(PendingSyncEntry {
                path: source.path.clone(),
                change: PendingSyncChange::Modified,
                source: Some(source.source),
                target,
                apply: false,
            });
        }
    }

    for include_path in sync.include_paths {
        if SYNC_EXCLUDES.contains(&include_path.as_str()) {
            continue;
        }
        let source_root = run_dir.join(&include_path);
        if !source_root.exists() {
            continue;
        }
        collect_added_sync_entries(
            &source_root,
            &include_path,
            side,
            &source_by_path,
            &mut seen,
            &mut entries,
            &mut ignored_generated,
        )?;
    }

    entries.sort_by(|left, right| {
        left.change
            .to_string()
            .cmp(&right.change.to_string())
            .then_with(|| left.path.cmp(&right.path))
    });
    let pending = PendingSyncManifest {
        side,
        run_dir: run_dir.to_path_buf(),
        path: pending_sync_path(project_dir, side),
        ignored_debug,
        ignored_generated,
        entries,
    };
    pending.write()?;
    Ok(pending)
}

pub fn apply_pending_sync(
    project_dir: &Path,
    pending: &mut PendingSyncManifest,
) -> Result<Vec<String>, RunError> {
    let mut applied = Vec::new();
    let mut remaining = Vec::new();
    for entry in pending.entries.drain(..) {
        if !entry.apply {
            remaining.push(entry);
            continue;
        }
        let target = project_dir
            .join("overrides")
            .join(entry.target.dir_name())
            .join(&entry.path);
        match entry.change {
            PendingSyncChange::Modified | PendingSyncChange::Added => {
                let source = pending.run_dir.join(&entry.path);
                if let Some(parent) = target.parent() {
                    create_dir(parent)?;
                }
                fs::copy(&source, &target).map_err(|source_err| RunError::CopyFile {
                    from: source.display().to_string(),
                    to: target.display().to_string(),
                    source: source_err,
                })?;
            }
            PendingSyncChange::Deleted => {
                if target.exists() {
                    fs::remove_file(&target).map_err(|source| RunError::RemoveFile {
                        path: target.display().to_string(),
                        source,
                    })?;
                }
            }
        }
        applied.push(format!(
            "{} {} -> {}",
            entry.change, entry.path, entry.target
        ));
    }
    pending.entries = remaining;
    pending.write()?;
    Ok(applied)
}

pub fn offline_uuid_for_player(player_name: &str) -> String {
    let mut digest = md5_digest(format!("OfflinePlayer:{player_name}").as_bytes());
    digest[6] = (digest[6] & 0x0f) | 0x30;
    digest[8] = (digest[8] & 0x3f) | 0x80;
    format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        digest[0],
        digest[1],
        digest[2],
        digest[3],
        digest[4],
        digest[5],
        digest[6],
        digest[7],
        digest[8],
        digest[9],
        digest[10],
        digest[11],
        digest[12],
        digest[13],
        digest[14],
        digest[15]
    )
}

pub fn curse_maven_artifact_url(file: &CurseMavenFile) -> String {
    let extension = extension_of(&file.original_file_name);
    format!(
        "https://www.cursemaven.com/curse/maven/{slug}-{project_id}/{file_id}/{slug}-{project_id}-{file_id}.{extension}",
        slug = file.slug,
        project_id = file.project_id,
        file_id = file.file_id,
    )
}

pub fn ordered_download_sources(source: DownloadSource, cn_probe: bool) -> Vec<DownloadSource> {
    match source {
        DownloadSource::Official => vec![DownloadSource::Official, DownloadSource::Bmclapi],
        DownloadSource::Bmclapi => vec![DownloadSource::Bmclapi, DownloadSource::Official],
        DownloadSource::Auto if cn_probe => vec![DownloadSource::Bmclapi, DownloadSource::Official],
        DownloadSource::Auto => vec![DownloadSource::Official, DownloadSource::Bmclapi],
    }
}

pub fn resolve_java_executable(
    project_dir: &Path,
    config: &crate::config::TestRunConfig,
    locator: &impl JavaLocator,
) -> Result<PathBuf, RunError> {
    if !config.java_path.trim().is_empty() {
        let path = PathBuf::from(config.java_path.trim());
        if path.is_file() {
            return Ok(path);
        }
        return Err(RunError::JavaNotFound {
            major: config.java_version_major,
        });
    }

    if let Some(path) = locator.path_java(config.java_version_major) {
        return Ok(path);
    }

    if let Some(path) = locator.common_java(config.java_version_major) {
        return Ok(path);
    }

    for path in project_java_candidates(project_dir, config.java_version_major) {
        if path.is_file() {
            return Ok(path);
        }
    }

    Err(RunError::JavaNotFound {
        major: config.java_version_major,
    })
}

pub fn save_zulu_java_download(
    project_dir: &Path,
    major: u16,
    url: &str,
    downloader: &impl DownloadBytes,
) -> Result<PathBuf, RunError> {
    let target = project_dir.join(format!(".mcmpu/java/{major}/zulu.zip"));
    if let Some(parent) = target.parent() {
        create_dir(parent)?;
    }
    let bytes = downloader.download_bytes(url)?;
    fs::write(&target, bytes).map_err(|source| RunError::WriteFile {
        path: target.display().to_string(),
        source,
    })?;
    Ok(target)
}

pub fn plan_launch_command(
    plan: &RunLaunchPlan,
    java_executable: &Path,
) -> Result<LaunchCommand, RunError> {
    let mut args = plan.java_args.clone();
    match plan.side {
        RunSide::Client => {
            args.extend([
                "-Dminecraft.launcher.brand=mcmpu".to_string(),
                "-Dminecraft.launcher.version=0.1.0".to_string(),
                "--username".to_string(),
                plan.player_name.clone(),
                "--uuid".to_string(),
                plan.offline_uuid.clone(),
                "--accessToken".to_string(),
                "0".to_string(),
                "--userType".to_string(),
                "legacy".to_string(),
            ]);
        }
        RunSide::Server => {
            args.extend([
                "-jar".to_string(),
                "server.jar".to_string(),
                "nogui".to_string(),
            ]);
        }
    }
    Ok(LaunchCommand {
        side: plan.side,
        working_dir: plan.run_dir.clone(),
        executable: java_executable.to_path_buf(),
        args,
    })
}

pub fn run_launch_command(
    project_dir: &Path,
    command: &LaunchCommand,
    runner: &impl ProcessRunner,
) -> Result<PathBuf, RunError> {
    runner.run(command)?;
    let pending = scan_pending_sync(project_dir, &command.working_dir, command.side)?;
    Ok(pending.path)
}

pub fn offline_access_token() -> &'static str {
    OFFLINE_ACCESS_TOKEN
}

pub fn run_file_name(file: &CurseMavenFile) -> String {
    let extension = extension_of(&file.original_file_name);
    format!(
        "{}{}-{}-{}.{}",
        sort_prefix(&file.original_file_name),
        file.slug,
        file.project_id,
        file.file_id,
        extension
    )
}

fn runtime_files(
    project_dir: &Path,
    resolved: &RuntimeResolvedLists,
    side: RunSide,
) -> Result<Vec<RunFile>, RunError> {
    let entries = match side {
        RunSide::Client => resolved
            .common
            .iter()
            .chain(&resolved.client)
            .chain(&resolved.resourcepacks)
            .collect::<Vec<_>>(),
        RunSide::Server => resolved
            .common
            .iter()
            .chain(&resolved.server)
            .chain(
                resolved
                    .resourcepacks
                    .iter()
                    .filter(|entry| entry.entry.dependency_type == DependencyType::ResourcePack),
            )
            .collect::<Vec<_>>(),
    };
    entries
        .into_iter()
        .map(|entry| runtime_file(project_dir, entry))
        .collect()
}

fn runtime_file(project_dir: &Path, resolved: &RuntimeResolvedEntry) -> Result<RunFile, RunError> {
    let project_id = resolved
        .entry
        .project_id
        .ok_or(RunError::MissingCurseMavenCoordinate)?;
    let file_id = resolved
        .entry
        .file_id
        .ok_or(RunError::MissingCurseMavenCoordinate)?;
    let slug = resolved
        .entry
        .slug
        .as_deref()
        .ok_or(RunError::MissingCurseMavenCoordinate)?;
    let artifact = CurseMavenFile::new(slug, &resolved.file)?;
    let file_name = run_file_name(&artifact);
    let folder = match resolved.entry.dependency_type {
        DependencyType::Mod => "mods",
        DependencyType::ResourcePack => "resourcepacks",
        DependencyType::ShaderPack => "shaderpacks",
    };
    let sha1 = resolved
        .file
        .hashes
        .iter()
        .find(|hash| hash.algo == 1)
        .map(|hash| hash.value.clone())
        .ok_or(RunError::MissingSha1 {
            project_id,
            file_id,
        })?;
    Ok(RunFile {
        path: format!("{folder}/{file_name}"),
        url: curse_maven_artifact_url(&artifact),
        cache_path: project_dir
            .join(".mcmpu")
            .join("cache")
            .join(folder)
            .join(file_name),
        sha1,
        size: resolved.file.file_length,
    })
}

fn java_args_for_side(config: &Config, side: RunSide) -> Vec<String> {
    let java = match side {
        RunSide::Client => &config.java.client,
        RunSide::Server => &config.java.server,
    };
    java_memory_args(java)
        .into_iter()
        .chain(config.test_run.debug_extra_jvm_args.clone())
        .collect()
}

fn java_memory_args(java: &JavaMemoryConfig) -> Vec<String> {
    let mut args = vec![
        format!("-Xms{}", java.min_memory),
        format!("-Xmx{}", java.max_memory),
    ];
    args.extend(java.extra_args.clone());
    args
}

fn rebuild_generated_dirs(run_dir: &Path) -> Result<(), RunError> {
    for dir in GENERATED_DIRS {
        let path = run_dir.join(dir);
        if path.exists() {
            fs::remove_dir_all(&path).map_err(|source| RunError::RemoveDir {
                path: path.display().to_string(),
                source,
            })?;
        }
        create_dir(&path)?;
    }
    Ok(())
}

fn copy_overrides(
    project_dir: &Path,
    run_dir: &Path,
    side: RunSide,
) -> Result<BTreeMap<String, RunSourceEntry>, RunError> {
    let mut sources = BTreeMap::new();
    copy_tree_if_exists(
        &project_dir.join("overrides").join("common"),
        run_dir,
        RunSourceKind::OverridesCommon,
        &mut sources,
    )?;
    copy_tree_if_exists(
        &project_dir.join("overrides").join(side.dir_name()),
        run_dir,
        match side {
            RunSide::Client => RunSourceKind::OverridesClient,
            RunSide::Server => RunSourceKind::OverridesServer,
        },
        &mut sources,
    )?;
    copy_tree_if_exists(
        &project_dir.join("debug").join("common"),
        run_dir,
        RunSourceKind::DebugCommon,
        &mut sources,
    )?;
    copy_tree_if_exists(
        &project_dir.join("debug").join(side.dir_name()),
        run_dir,
        match side {
            RunSide::Client => RunSourceKind::DebugClient,
            RunSide::Server => RunSourceKind::DebugServer,
        },
        &mut sources,
    )?;
    Ok(sources)
}

fn copy_tree_if_exists(
    source: &Path,
    target_root: &Path,
    source_kind: RunSourceKind,
    sources: &mut BTreeMap<String, RunSourceEntry>,
) -> Result<(), RunError> {
    if !source.exists() {
        return Ok(());
    }
    for entry in WalkDir::new(source) {
        let entry = entry.map_err(|source_err| RunError::WalkDir {
            path: source.display().to_string(),
            source: source_err,
        })?;
        if entry.file_type().is_dir() {
            continue;
        }
        let relative = entry
            .path()
            .strip_prefix(source)
            .expect("walked under source");
        let target = target_root.join(relative);
        if let Some(parent) = target.parent() {
            create_dir(parent)?;
        }
        fs::copy(entry.path(), &target).map_err(|source_err| RunError::CopyFile {
            from: entry.path().display().to_string(),
            to: target.display().to_string(),
            source: source_err,
        })?;
        let fingerprint = file_fingerprint(&target)?;
        let path = slash_path(relative);
        sources.insert(
            path.clone(),
            RunSourceEntry {
                path,
                source: source_kind,
                target: source_kind.target(),
                source_path: entry.path().display().to_string(),
                sha1: fingerprint.sha1,
                size: fingerprint.size,
            },
        );
    }
    Ok(())
}

fn write_source_manifest(
    run_dir: &Path,
    side: RunSide,
    sources: BTreeMap<String, RunSourceEntry>,
) -> Result<(), RunError> {
    let manifest = RunSourceManifest {
        side,
        entries: sources.into_values().collect(),
    };
    let path = run_dir.join(RUN_SOURCES_FILE);
    let text = serde_json::to_string_pretty(&manifest)?;
    fs::write(&path, text).map_err(|source| RunError::WriteFile {
        path: path.display().to_string(),
        source,
    })
}

fn ensure_sync_config(project_dir: &Path, run_dir: &Path, side: RunSide) -> Result<(), RunError> {
    let sync_path = run_dir.join(".mcmpu-sync.toml");
    if sync_path.exists() {
        let _ = SyncConfig::from_path(&sync_path)?;
        return Ok(());
    }
    let config = SyncConfig {
        include_paths: default_sync_include_paths(project_dir, side)?,
    };
    let text = toml::to_string_pretty(&config)?;
    fs::write(&sync_path, text).map_err(|source| RunError::WriteFile {
        path: sync_path.display().to_string(),
        source,
    })
}

fn default_sync_include_paths(project_dir: &Path, side: RunSide) -> Result<Vec<String>, RunError> {
    let mut paths = BTreeSet::new();
    collect_top_level_dirs(&project_dir.join("overrides").join("common"), &mut paths)?;
    collect_top_level_dirs(
        &project_dir.join("overrides").join(side.dir_name()),
        &mut paths,
    )?;
    Ok(paths
        .into_iter()
        .filter(|path| !SYNC_EXCLUDES.contains(&path.as_str()))
        .collect())
}

fn collect_top_level_dirs(root: &Path, paths: &mut BTreeSet<String>) -> Result<(), RunError> {
    if !root.exists() {
        return Ok(());
    }
    for entry in fs::read_dir(root).map_err(|source| RunError::ReadSyncConfig {
        path: root.display().to_string(),
        source,
    })? {
        let entry = entry.map_err(|source| RunError::ReadSyncConfig {
            path: root.display().to_string(),
            source,
        })?;
        if entry
            .file_type()
            .map(|file_type| file_type.is_dir())
            .unwrap_or(false)
        {
            paths.insert(entry.file_name().to_string_lossy().to_string());
        }
    }
    Ok(())
}

fn write_run_manifest(run_dir: &Path, files: &[RunFile]) -> Result<(), RunError> {
    let manifest_path = run_dir.join(".mcmpu-run-files.json");
    let text = serde_json::to_string_pretty(files).map_err(|source| RunError::WriteFile {
        path: manifest_path.display().to_string(),
        source: std::io::Error::other(source),
    })?;
    fs::write(&manifest_path, text).map_err(|source| RunError::WriteFile {
        path: manifest_path.display().to_string(),
        source,
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct FileFingerprint {
    sha1: String,
    size: u64,
}

fn file_fingerprint(path: &Path) -> Result<FileFingerprint, RunError> {
    let bytes = fs::read(path).map_err(|source| RunError::ReadFile {
        path: path.display().to_string(),
        source,
    })?;
    Ok(FileFingerprint {
        sha1: sha1_hex(&bytes),
        size: bytes.len() as u64,
    })
}

fn is_valid_file(path: &Path, expected_size: u64, expected_sha1: &str) -> Result<bool, RunError> {
    if !path.exists() {
        return Ok(false);
    }
    let bytes = fs::read(path).map_err(|source| RunError::ReadFile {
        path: path.display().to_string(),
        source,
    })?;
    Ok(bytes.len() as u64 == expected_size && sha1_hex(&bytes).eq_ignore_ascii_case(expected_sha1))
}

fn verify_bytes(
    path: &Path,
    bytes: &[u8],
    expected_size: u64,
    expected_sha1: &str,
) -> Result<(), RunError> {
    let actual_size = bytes.len() as u64;
    if actual_size != expected_size {
        return Err(RunError::SizeMismatch {
            path: path.display().to_string(),
            expected: expected_size,
            actual: actual_size,
        });
    }
    let actual_sha1 = sha1_hex(bytes);
    if !actual_sha1.eq_ignore_ascii_case(expected_sha1) {
        return Err(RunError::Sha1Mismatch {
            path: path.display().to_string(),
            expected: expected_sha1.to_string(),
            actual: actual_sha1,
        });
    }
    Ok(())
}

fn sha1_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha1::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

fn collect_added_sync_entries(
    source_root: &Path,
    include_path: &str,
    side: RunSide,
    source_by_path: &BTreeMap<String, &RunSourceEntry>,
    seen: &mut BTreeSet<String>,
    entries: &mut Vec<PendingSyncEntry>,
    ignored_generated: &mut usize,
) -> Result<(), RunError> {
    for entry in WalkDir::new(source_root) {
        let entry = entry.map_err(|source| RunError::WalkDir {
            path: source_root.display().to_string(),
            source,
        })?;
        if entry.file_type().is_dir() {
            continue;
        }
        let relative = entry
            .path()
            .strip_prefix(source_root)
            .expect("walked path under source root");
        let path = format!("{include_path}/{}", slash_path(relative));
        if source_by_path.contains_key(&path) || !seen.insert(path.clone()) {
            continue;
        }
        if is_generated_or_internal_path(&path) {
            *ignored_generated += 1;
            continue;
        }
        entries.push(PendingSyncEntry {
            path,
            change: PendingSyncChange::Added,
            source: None,
            target: SyncTarget::from_run_side(side),
            apply: false,
        });
    }
    Ok(())
}

fn pending_sync_path(project_dir: &Path, side: RunSide) -> PathBuf {
    project_dir
        .join(".mcmpu")
        .join(side.pending_sync_file_name())
}

fn is_generated_or_internal_path(path: &str) -> bool {
    path.split('/')
        .next()
        .is_some_and(|part| SYNC_EXCLUDES.contains(&part))
}

fn create_dir(path: impl AsRef<Path>) -> Result<(), RunError> {
    let path = path.as_ref();
    fs::create_dir_all(path).map_err(|source| RunError::CreateDir {
        path: path.display().to_string(),
        source,
    })
}

fn slash_path(path: &Path) -> String {
    path.components()
        .map(|component| component.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

fn java_executable_name() -> &'static str {
    if cfg!(windows) { "java.exe" } else { "java" }
}

fn find_on_path(name: &str) -> Option<PathBuf> {
    let path_var = env::var_os("PATH")?;
    env::split_paths(&path_var)
        .map(|dir| dir.join(name))
        .find(|candidate| candidate.is_file())
}

fn common_java_candidates(major: u16) -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    let exe = java_executable_name();
    if cfg!(windows) {
        for root in ["ProgramFiles", "ProgramFiles(x86)"] {
            if let Some(base) = env::var_os(root).map(PathBuf::from) {
                candidates.push(base.join(format!("Eclipse Adoptium/jdk-{major}/bin/{exe}")));
                candidates.push(base.join(format!("Zulu/zulu-{major}/bin/{exe}")));
                candidates.push(base.join(format!("Java/jdk-{major}/bin/{exe}")));
            }
        }
    } else if cfg!(target_os = "macos") {
        candidates.push(PathBuf::from(format!(
            "/Library/Java/JavaVirtualMachines/zulu-{major}.jdk/Contents/Home/bin/{exe}"
        )));
        candidates.push(PathBuf::from(format!(
            "/Library/Java/JavaVirtualMachines/temurin-{major}.jdk/Contents/Home/bin/{exe}"
        )));
    } else {
        candidates.push(PathBuf::from(format!(
            "/usr/lib/jvm/java-{major}-openjdk/bin/{exe}"
        )));
        candidates.push(PathBuf::from(format!(
            "/usr/lib/jvm/zulu-{major}/bin/{exe}"
        )));
    }
    candidates
}

fn project_java_candidates(project_dir: &Path, major: u16) -> Vec<PathBuf> {
    let exe = java_executable_name();
    let mut candidates = vec![
        project_dir.join(format!(".mcmpu/java/{major}/bin/{exe}")),
        project_dir.join(format!(".mcmpu/java/{major}/{exe}")),
    ];
    if exe != "java" {
        candidates.push(project_dir.join(format!(".mcmpu/java/{major}/bin/java")));
        candidates.push(project_dir.join(format!(".mcmpu/java/{major}/java")));
    }
    candidates
}

fn extension_of(file_name: &str) -> String {
    Path::new(file_name)
        .extension()
        .and_then(|extension| extension.to_str())
        .unwrap_or("jar")
        .to_ascii_lowercase()
}

fn sort_prefix(file_name: &str) -> String {
    file_name
        .chars()
        .take_while(|ch| !ch.is_ascii_alphanumeric())
        .collect()
}

fn md5_digest(input: &[u8]) -> [u8; 16] {
    const S: [u32; 64] = [
        7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 5, 9, 14, 20, 5, 9, 14, 20, 5,
        9, 14, 20, 5, 9, 14, 20, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 6, 10,
        15, 21, 6, 10, 15, 21, 6, 10, 15, 21, 6, 10, 15, 21,
    ];
    const K: [u32; 64] = [
        0xd76aa478, 0xe8c7b756, 0x242070db, 0xc1bdceee, 0xf57c0faf, 0x4787c62a, 0xa8304613,
        0xfd469501, 0x698098d8, 0x8b44f7af, 0xffff5bb1, 0x895cd7be, 0x6b901122, 0xfd987193,
        0xa679438e, 0x49b40821, 0xf61e2562, 0xc040b340, 0x265e5a51, 0xe9b6c7aa, 0xd62f105d,
        0x02441453, 0xd8a1e681, 0xe7d3fbc8, 0x21e1cde6, 0xc33707d6, 0xf4d50d87, 0x455a14ed,
        0xa9e3e905, 0xfcefa3f8, 0x676f02d9, 0x8d2a4c8a, 0xfffa3942, 0x8771f681, 0x6d9d6122,
        0xfde5380c, 0xa4beea44, 0x4bdecfa9, 0xf6bb4b60, 0xbebfbc70, 0x289b7ec6, 0xeaa127fa,
        0xd4ef3085, 0x04881d05, 0xd9d4d039, 0xe6db99e5, 0x1fa27cf8, 0xc4ac5665, 0xf4292244,
        0x432aff97, 0xab9423a7, 0xfc93a039, 0x655b59c3, 0x8f0ccc92, 0xffeff47d, 0x85845dd1,
        0x6fa87e4f, 0xfe2ce6e0, 0xa3014314, 0x4e0811a1, 0xf7537e82, 0xbd3af235, 0x2ad7d2bb,
        0xeb86d391,
    ];

    let bit_len = (input.len() as u64) * 8;
    let mut data = input.to_vec();
    data.push(0x80);
    while data.len() % 64 != 56 {
        data.push(0);
    }
    data.extend(bit_len.to_le_bytes());

    let mut a0 = 0x67452301u32;
    let mut b0 = 0xefcdab89u32;
    let mut c0 = 0x98badcfeu32;
    let mut d0 = 0x10325476u32;

    for chunk in data.chunks_exact(64) {
        let mut m = [0u32; 16];
        for (index, word) in m.iter_mut().enumerate() {
            let start = index * 4;
            *word = u32::from_le_bytes([
                chunk[start],
                chunk[start + 1],
                chunk[start + 2],
                chunk[start + 3],
            ]);
        }

        let mut a = a0;
        let mut b = b0;
        let mut c = c0;
        let mut d = d0;

        for i in 0..64 {
            let (f, g) = match i {
                0..=15 => ((b & c) | ((!b) & d), i),
                16..=31 => ((d & b) | ((!d) & c), (5 * i + 1) % 16),
                32..=47 => (b ^ c ^ d, (3 * i + 5) % 16),
                _ => (c ^ (b | (!d)), (7 * i) % 16),
            };
            let next = b.wrapping_add(
                a.wrapping_add(f)
                    .wrapping_add(K[i])
                    .wrapping_add(m[g])
                    .rotate_left(S[i]),
            );
            a = d;
            d = c;
            c = b;
            b = next;
        }

        a0 = a0.wrapping_add(a);
        b0 = b0.wrapping_add(b);
        c0 = c0.wrapping_add(c);
        d0 = d0.wrapping_add(d);
    }

    let mut out = [0u8; 16];
    out[0..4].copy_from_slice(&a0.to_le_bytes());
    out[4..8].copy_from_slice(&b0.to_le_bytes());
    out[8..12].copy_from_slice(&c0.to_le_bytes());
    out[12..16].copy_from_slice(&d0.to_le_bytes());
    out
}
