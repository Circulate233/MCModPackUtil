use std::collections::{BTreeMap, BTreeSet};
use std::env;
#[cfg(windows)]
use std::ffi::OsString;
use std::fs::{self, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::ExitStatus;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use serde::{Deserialize, Serialize};
use sha1::{Digest, Sha1};
use thiserror::Error;
use walkdir::WalkDir;

use crate::config::{Config, ConfigError, DownloadSource, LoaderKind, TestRunConfig};
use crate::curseforge::client::CurseForgeClient;
use crate::curseforge::model::CfFile;
use crate::curseforge::resolver::CurseForgeApi;
use crate::dependency::DependencyType;
use crate::net::{
    ArtifactRequest, ArtifactTransfer, HttpRequest, HttpTransport, NetworkEngine, NetworkError,
    SensitiveHeaders, TransferError, TransferEvent, TransferObserver, TransferPhase,
};
use crate::path_safety::validate_portable_relative_path;
use crate::project::artifact_install::{
    ArtifactDownloadProgress, ArtifactDownloadRequest, ArtifactDownloader, ArtifactInstallError,
    ArtifactInstallProgress, ArtifactInstallSource, ArtifactInstaller, ArtifactInstallerImpl,
    ArtifactProgressObserver,
};
use crate::project::build::{
    BuildError, RuntimeResolvedEntry, RuntimeResolvedLists, resolve_project_for_runtime_with_api,
};
use crate::project::jvm_args::{append_extra_arguments, java_memory_arguments};
use crate::project::launcher::{LaunchDescriptor, LaunchResolveError, LaunchSide};
pub use crate::project::process::{
    ProcessError, ProcessOutputStream, ProcessRunner, ProcessSpec, RunningProcess,
    SystemProcessRunner,
};
use crate::project::runtime_install::{
    RuntimeInstallError, RuntimeInstallProgress, RuntimeInstallStream, RuntimeProfileLocator,
    RuntimeProfileLocatorImpl, profile_source,
};

const GENERATED_DIRS: &[&str] = &["mods", "resourcepacks", "shaderpacks"];
const SYNC_EXCLUDES: &[&str] = &[
    "mods",
    "resourcepacks",
    "shaderpacks",
    "libraries",
    "versions",
    "assets",
    "natives",
    ".mcmpu",
];
const OFFLINE_ACCESS_TOKEN: &str = "0";
/// Dedicated-server files mcmpu has to control before the JVM can start.
const EULA_FILE: &str = "eula.txt";
const SERVER_PROPERTIES_FILE: &str = "server.properties";
/// Header written above a freshly created `eula.txt` so the accepted agreement stays visible.
const EULA_FILE_HEADER: &str = "# accepted by mcmpu for local development runs\n# Minecraft EULA: https://aka.ms/MinecraftEULA";
const RUN_SOURCES_FILE: &str = ".mcmpu-run-sources.json";
const RUN_SUPERVISOR_POLL_INTERVAL: Duration = Duration::from_millis(50);
const RUN_DOWNLOAD_PROGRESS_INTERVAL: Duration = Duration::from_millis(500);
const RUN_DOWNLOAD_EVENT_QUEUE: usize = 512;
const GRACEFUL_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(10);
const CACHE_LOCK_WAIT_TIMEOUT: Duration = Duration::from_secs(120);
const MAX_METADATA_RESPONSE_BYTES: usize = 16 * 1024 * 1024;

static CTRL_C_LISTENERS: OnceLock<Mutex<Vec<Weak<AtomicBool>>>> = OnceLock::new();
static CTRL_C_HANDLER_RESULT: OnceLock<Result<(), String>> = OnceLock::new();
static CACHE_TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

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

impl From<&LaunchCommand> for ProcessSpec {
    fn from(command: &LaunchCommand) -> Self {
        Self {
            executable: command.executable.clone(),
            args: command.args.clone(),
            working_dir: command.working_dir.clone(),
            suppress_console_window: true,
        }
    }
}

/// All resolved inputs required to launch and supervise one game process.
#[derive(Debug, Clone)]
pub struct RunRequest {
    /// Project root whose pending sync manifest is updated after the run.
    pub project_dir: PathBuf,
    /// Prepared run directory, memory arguments, identity, and runtime files.
    pub launch_plan: RunLaunchPlan,
    /// Loader-resolved JVM, classpath, main-class, and game arguments.
    pub launch_descriptor: LaunchDescriptor,
    /// Java executable selected for the descriptor's required major version.
    pub java_executable: PathBuf,
    /// Delay between non-blocking process status checks.
    pub poll_interval: Duration,
    /// Maximum graceful-stop period before forced termination.
    pub graceful_shutdown_timeout: Duration,
}

/// Why a supervised game process stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunTermination {
    /// The process exited without a stop request from the caller.
    Exited,
    /// The process exited after the caller requested shutdown.
    StopRequested,
    /// The graceful shutdown deadline expired and the process was killed.
    Forced,
}

/// Final state returned after the process and pending sync lifecycle complete.
#[derive(Debug)]
pub struct RunOutcome {
    /// Operating-system identifier of the supervised process.
    pub pid: u32,
    /// Exit status returned after the child was reaped.
    pub status: ExitStatus,
    /// Supervisor reason associated with the process exit.
    pub termination: RunTermination,
    /// Pending changes scanned and written after process completion.
    pub pending_sync: PendingSyncManifest,
}

/// Observable transitions emitted by the run supervisor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunEvent {
    /// Process creation is about to be attempted with this diagnostic command.
    Spawning { command: String },
    /// The operating system created the child process.
    Spawned { pid: u32 },
    /// The caller requested that the child stop.
    StopRequested { pid: u32 },
    /// The graceful shutdown deadline expired and termination was forced.
    ForcedTermination { pid: u32 },
    /// The child was reaped with the indicated success state.
    Exited { pid: u32, success: bool },
    /// Pending sync changes were scanned and written to disk.
    PendingSyncWritten { path: PathBuf, entries: usize },
    /// A line emitted by the running Minecraft JVM.
    ProcessOutput {
        /// Operating-system identifier of the process that emitted the line.
        pid: u32,
        /// Child standard stream that emitted the line.
        stream: ProcessOutputStream,
        /// Lossily decoded complete output line.
        line: String,
    },
}

/// Supplies cancellation state without coupling the lifecycle to a UI or signal crate.
pub trait StopSignal {
    /// Returns `true` once the caller wants the supervised process to stop.
    fn stop_requested(&self) -> bool;
}

/// A cloneable cancellation flag that lets an external event stop a supervised run.
///
/// The flag is deliberately independent from the platform signal integration so GUI code and
/// tests can control a run without registering a process-global Ctrl+C handler.
#[derive(Debug, Clone, Default)]
pub struct SharedStopSignal {
    requested: Arc<AtomicBool>,
}

impl SharedStopSignal {
    /// Requests shutdown for every supervisor sharing this signal.
    pub fn request_stop(&self) {
        self.requested.store(true, Ordering::SeqCst);
    }
}

impl StopSignal for SharedStopSignal {
    fn stop_requested(&self) -> bool {
        self.requested.load(Ordering::SeqCst)
    }
}

/// Supplies console lines typed while a run is in progress.
pub trait ConsoleInput {
    /// Takes every line submitted since the previous call without blocking.
    fn take_lines(&self) -> Vec<String>;
}

/// Console input that never yields lines, used when a run has no attached console.
#[derive(Debug, Default)]
pub struct NeverConsole;

impl ConsoleInput for NeverConsole {
    fn take_lines(&self) -> Vec<String> {
        Vec::new()
    }
}

/// Console buffer a user interface fills and the run supervisor drains.
///
/// A run is supervised on its own thread while the UI keeps rendering, so typed lines are queued
/// here and forwarded to the child's standard input by the supervisor loop.
#[derive(Debug, Clone, Default)]
pub struct SharedConsoleInput {
    pending: Arc<Mutex<Vec<String>>>,
}

impl SharedConsoleInput {
    /// Queues one console line for the running process, ignoring blank input.
    pub fn submit(&self, line: impl AsRef<str>) {
        let line = line.as_ref().trim_end_matches(['\r', '\n']);
        if line.trim().is_empty() {
            return;
        }
        if let Ok(mut pending) = self.pending.lock() {
            pending.push(line.to_string());
        }
    }
}

impl ConsoleInput for SharedConsoleInput {
    fn take_lines(&self) -> Vec<String> {
        self.pending
            .lock()
            .map(|mut pending| std::mem::take(&mut *pending))
            .unwrap_or_default()
    }
}

/// Installs the process-wide Ctrl+C handler and returns a cancellation flag for one CLI run.
///
/// The handler is installed at most once because operating systems expose Ctrl+C as a global
/// process event. Each call registers a weak listener, so completed runs do not keep state alive.
pub fn install_cli_ctrl_c_signal() -> Result<SharedStopSignal, RunError> {
    let install = CTRL_C_HANDLER_RESULT.get_or_init(|| {
        ctrlc::set_handler(notify_ctrl_c_listeners).map_err(|error| error.to_string())
    });
    if let Err(reason) = install {
        return Err(RunError::InterruptHandler {
            reason: reason.clone(),
        });
    }

    let signal = SharedStopSignal::default();
    let listeners = CTRL_C_LISTENERS.get_or_init(|| Mutex::new(Vec::new()));
    let mut listeners = listeners.lock().map_err(|_| RunError::InterruptHandler {
        reason: "Ctrl+C listener registry lock is poisoned".to_string(),
    })?;
    listeners.push(Arc::downgrade(&signal.requested));
    Ok(signal)
}

fn notify_ctrl_c_listeners() {
    let Some(listeners) = CTRL_C_LISTENERS.get() else {
        return;
    };
    let Ok(mut listeners) = listeners.lock() else {
        eprintln!("[run] Ctrl+C listener registry is unavailable");
        return;
    };
    listeners.retain(|listener| {
        let Some(listener) = listener.upgrade() else {
            return false;
        };
        listener.store(true, Ordering::SeqCst);
        true
    });
}

/// Stop signal used for blocking command compatibility entry points.
#[derive(Debug, Clone, Copy, Default)]
pub struct NeverStop;

impl StopSignal for NeverStop {
    fn stop_requested(&self) -> bool {
        false
    }
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
    CopyingDebug {
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
    VerifyingFiles {
        total: usize,
    },
    FileVerificationProgress {
        checked: usize,
        total: usize,
    },
    RunFilesChecked {
        reused: usize,
        cached: usize,
        downloading: usize,
        download_bytes: u64,
    },
    DownloadBatchStarted {
        total: usize,
        total_bytes: u64,
    },
    FileDownloadStarted {
        started: usize,
        total: usize,
        path: String,
    },
    DownloadProgress {
        completed: usize,
        total: usize,
        transferred_bytes: u64,
        total_bytes: u64,
        bytes_per_second: u64,
        active_requests: usize,
    },
    FileReady {
        completed: usize,
        total: usize,
        path: String,
        source: RunFileReadySource,
    },
    ResolvingJava {
        major: u16,
    },
    JavaResolved {
        path: PathBuf,
    },
    DownloadingLoaderInstaller {
        side: RunSide,
        url: String,
        target: PathBuf,
    },
    LoaderInstallerDownloaded {
        side: RunSide,
        target: PathBuf,
    },
    RunningLoaderInstaller {
        side: RunSide,
        loader: LoaderKind,
    },
    LoaderInstallerOutput {
        side: RunSide,
        stream: RuntimeInstallStream,
        line: String,
    },
    InstallingLoaderLibraries {
        side: RunSide,
        total: usize,
    },
    LoaderLibraryReady {
        side: RunSide,
        completed: usize,
        total: usize,
        path: String,
    },
    PreparingRuntimeArtifacts {
        side: RunSide,
        total: usize,
    },
    RuntimeArtifactTotalUpdated {
        side: RunSide,
        total: usize,
    },
    RuntimeArtifactReady {
        side: RunSide,
        completed: usize,
        total: usize,
        path: String,
        source: ArtifactInstallSource,
    },
    RuntimeArtifactDownloadProgress {
        side: RunSide,
        transferred_bytes: u64,
        total_bytes: u64,
        active_requests: usize,
    },
    Launching {
        command: String,
    },
    MinecraftOutput {
        side: RunSide,
        stream: ProcessOutputStream,
        line: String,
    },
    ServerRunConfigurationWritten {
        /// Names of the server files that had to be created or corrected.
        changed: Vec<String>,
    },
    PendingSyncWritten {
        path: PathBuf,
        entries: usize,
    },
    PendingSyncFailed {
        path: PathBuf,
        error: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunFileReadySource {
    Reused,
    Cache,
    Gradle,
    Downloaded,
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
        write_bytes_atomically(&self.path, text.as_bytes())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SyncConfig {
    pub include_paths: Vec<String>,
}

pub trait DownloadBytes: Sync {
    fn download_bytes(&self, url: &str) -> Result<Vec<u8>, RunError>;

    fn download_many(&self, requests: &[RunDownloadRequest]) -> Vec<Result<(), RunError>> {
        requests
            .iter()
            .map(|request| {
                let bytes = self.download_bytes(&request.url)?;
                verify_run_download_bytes(request, &bytes)?;
                write_bytes_atomically(&request.target, &bytes)
            })
            .collect()
    }

    fn download_many_observed(
        &self,
        requests: &[RunDownloadRequest],
        observer: Arc<dyn TransferObserver>,
    ) -> Vec<Result<(), RunError>> {
        for request in requests {
            observer.observe(TransferEvent {
                task_id: request.task_id.clone(),
                phase: TransferPhase::Queued,
                transferred_bytes: 0,
                total_bytes: request.expected_size,
                active_requests: 0,
                bytes_per_second: 0.0,
            });
        }
        let results = self.download_many(requests);
        for (request, result) in requests.iter().zip(&results) {
            observer.observe(TransferEvent {
                task_id: request.task_id.clone(),
                phase: if result.is_ok() {
                    TransferPhase::Completed
                } else {
                    TransferPhase::Failed
                },
                transferred_bytes: result.as_ref().ok().and(request.expected_size).unwrap_or(0),
                total_bytes: request.expected_size,
                active_requests: 0,
                bytes_per_second: 0.0,
            });
        }
        results
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunDownloadRequest {
    pub task_id: String,
    pub url: String,
    pub fallback_urls: Vec<String>,
    pub target: PathBuf,
    pub expected_size: Option<u64>,
    pub expected_sha1: Option<String>,
    pub expected_sha256: Option<String>,
}

#[derive(Clone)]
pub struct HttpDownloader {
    engine: Arc<NetworkEngine>,
    curseforge_api_key: Option<String>,
    sensitive_headers: SensitiveHeaders,
}

pub trait JavaLocator {
    fn path_java(&self, major: u16) -> Option<PathBuf>;
    fn common_java(&self, major: u16) -> Option<PathBuf>;
}

#[derive(Debug, Default)]
pub struct SystemJavaLocator;

#[derive(Debug, Error)]
pub enum RunError {
    #[error(transparent)]
    Build(#[from] BuildError),
    #[error(transparent)]
    Config(#[from] ConfigError),
    #[error(transparent)]
    Launch(#[from] LaunchResolveError),
    #[error(transparent)]
    RuntimeInstall(#[from] RuntimeInstallError),
    #[error(transparent)]
    ArtifactInstall(#[from] ArtifactInstallError),
    #[error(transparent)]
    Process(#[from] ProcessError),
    #[error("dependency is missing project_id, file_id, or slug for its CurseForge runtime file")]
    MissingCurseForgeRuntimeCoordinate,
    #[error("CurseForge file {project_id}/{file_id} has an empty file name")]
    EmptyCurseForgeFileName { project_id: u64, file_id: u64 },
    #[error("CurseForge CDN download requires an API key for '{url}'")]
    MissingCurseForgeDownloadKey { url: String },
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
    #[error(transparent)]
    Network(#[from] NetworkError),
    #[error("artifact transfer failed: {0}")]
    Transfer(#[from] TransferError),
    #[error("invalid artifact download batch: {reason}")]
    InvalidDownloadBatch { reason: String },
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
    #[error("failed to read directory '{path}': {source}")]
    ReadDir {
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
    #[error(
        "invalid sync include path '{path}': paths must be relative and cannot target generated run content"
    )]
    InvalidSyncIncludePath { path: String },
    #[error("invalid pending sync entry '{path}': {reason}")]
    InvalidPendingSyncEntry { path: String, reason: String },
    #[error("failed to serialize JSON: {0}")]
    SerializeJson(#[from] serde_json::Error),
    #[error("failed to walk directory '{path}': {source}")]
    WalkDir {
        path: String,
        #[source]
        source: walkdir::Error,
    },
    #[error(
        "Java major {major} executable was not found. Configure java_path in test-run.toml or install Java version {major}."
    )]
    JavaNotFound { major: u16 },
    #[error("Azul Zulu metadata could not be used: {reason}")]
    ZuluMetadata { reason: String },
    #[error("Zulu archive SHA-256 mismatch: expected {expected}, got {actual}")]
    ZuluChecksum { expected: String, actual: String },
    #[error("Zulu archive extraction failed: {reason}")]
    ZuluArchive { reason: String },
    #[error("timed out waiting for cache lock '{path}'")]
    CacheLockTimeout { path: String },
    #[error("run process exited with status {status}")]
    ProcessExit { status: std::process::ExitStatus },
    #[error("a real launch descriptor is required; username-only client launch was removed")]
    LaunchDescriptorRequired,
    #[error("runtime installation is unsupported on platform '{platform}'")]
    UnsupportedRuntimePlatform { platform: String },
    #[error("run was cancelled")]
    Cancelled,
    #[error("failed to install Ctrl+C run cancellation handler: {reason}")]
    InterruptHandler { reason: String },
    #[error("launch plan side {plan:?} does not match descriptor side {descriptor:?}")]
    LaunchSideMismatch {
        plan: RunSide,
        descriptor: LaunchSide,
    },
    #[error("launch descriptor main class must not be empty")]
    EmptyMainClass,
    #[error("invalid launch descriptor: {reason}")]
    InvalidLaunchDescriptor { reason: String },
    #[error("failed to resolve launch run directory '{path}': {source}")]
    LaunchRunDirectory {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to resolve launch Java executable '{path}': {source}")]
    LaunchJavaExecutable {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error(transparent)]
    JvmArguments(#[from] crate::project::jvm_args::JvmArgumentError),
    #[error("run lifecycle failed ({lifecycle}); pending sync also failed ({pending_sync})")]
    LifecycleAndPendingSync {
        #[source]
        lifecycle: Box<RunError>,
        pending_sync: Box<RunError>,
    },
}

impl HttpDownloader {
    pub fn new(proxy: Option<&str>) -> Result<Self, RunError> {
        Self::with_curseforge_api_key(proxy, None)
    }

    pub fn with_curseforge_api_key(
        proxy: Option<&str>,
        curseforge_api_key: Option<&str>,
    ) -> Result<Self, RunError> {
        let config = crate::net::NetworkConfig {
            proxy: proxy
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_owned),
            ..crate::net::NetworkConfig::default()
        };
        let engine = Arc::new(NetworkEngine::new(config)?);
        Self::with_network_engine(engine, curseforge_api_key)
    }

    pub fn with_network_engine(
        engine: Arc<NetworkEngine>,
        curseforge_api_key: Option<&str>,
    ) -> Result<Self, RunError> {
        let curseforge_api_key = curseforge_api_key
            .map(str::trim)
            .filter(|key| !key.is_empty())
            .map(ToOwned::to_owned);
        let mut sensitive_headers = SensitiveHeaders::new()
            .allow_origin("https://edge.forgecdn.net")?
            .allow_origin("https://mediafilez.forgecdn.net")?;
        if let Some(api_key) = &curseforge_api_key {
            sensitive_headers = sensitive_headers.insert(
                reqwest::header::HeaderName::from_static("x-api-key"),
                reqwest::header::HeaderValue::from_str(api_key).map_err(|error| {
                    NetworkError::InvalidConfiguration {
                        reason: format!("invalid CurseForge API key header: {error}"),
                    }
                })?,
            )?;
        }
        Ok(Self {
            engine,
            curseforge_api_key,
            sensitive_headers,
        })
    }
}

impl DownloadBytes for HttpDownloader {
    fn download_bytes(&self, url: &str) -> Result<Vec<u8>, RunError> {
        if is_curseforge_cdn_url(url) {
            self.curseforge_api_key.as_deref().ok_or_else(|| {
                RunError::MissingCurseForgeDownloadKey {
                    url: url.to_string(),
                }
            })?;
        }
        let request = HttpRequest::get(url, MAX_METADATA_RESPONSE_BYTES)
            .sensitive_headers(self.sensitive_headers.clone())
            .build()?;
        Ok(self.engine.get_bytes(request)?.body)
    }

    fn download_many(&self, requests: &[RunDownloadRequest]) -> Vec<Result<(), RunError>> {
        <Self as DownloadBytes>::download_many_observed(
            self,
            requests,
            Arc::new(|_: TransferEvent| {}),
        )
    }

    fn download_many_observed(
        &self,
        requests: &[RunDownloadRequest],
        observer: Arc<dyn TransferObserver>,
    ) -> Vec<Result<(), RunError>> {
        if self.curseforge_api_key.is_none()
            && let Some(url) = requests
                .iter()
                .flat_map(|request| std::iter::once(&request.url).chain(&request.fallback_urls))
                .find(|url| is_curseforge_cdn_url(url))
        {
            return requests
                .iter()
                .map(|_| Err(RunError::MissingCurseForgeDownloadKey { url: url.clone() }))
                .collect();
        }
        let built = requests
            .iter()
            .map(|request| build_network_artifact_request(request, &self.sensitive_headers))
            .collect::<Result<Vec<_>, _>>();
        let built = match built {
            Ok(built) => built,
            Err(error) => {
                let reason = error.to_string();
                return requests
                    .iter()
                    .map(|request| {
                        Err(RunError::InvalidDownloadBatch {
                            reason: format!(
                                "invalid artifact request '{}': {reason}",
                                request.task_id
                            ),
                        })
                    })
                    .collect();
            }
        };
        self.engine
            .transfer_many(built, observer)
            .into_iter()
            .map(|result| result.map(|_| ()).map_err(RunError::from))
            .collect()
    }
}

impl ArtifactDownloader for HttpDownloader {
    fn download(&self, url: &str) -> Result<Vec<u8>, ArtifactInstallError> {
        self.download_bytes(url)
            .map_err(|error| ArtifactInstallError::Download {
                url: url.to_string(),
                reason: error.to_string(),
            })
    }

    fn download_many(
        &self,
        requests: &[ArtifactDownloadRequest],
    ) -> Vec<Result<(), ArtifactInstallError>> {
        <Self as ArtifactDownloader>::download_many_observed(self, requests, Arc::new(|_| {}))
    }

    fn download_many_observed(
        &self,
        requests: &[ArtifactDownloadRequest],
        observer: Arc<ArtifactProgressObserver>,
    ) -> Vec<Result<(), ArtifactInstallError>> {
        let requests = requests
            .iter()
            .map(|request| RunDownloadRequest {
                task_id: request.task_id.clone(),
                url: request.url.clone(),
                fallback_urls: Vec::new(),
                target: request.target.clone(),
                expected_size: request.size,
                expected_sha1: Some(request.sha1.clone()),
                expected_sha256: None,
            })
            .collect::<Vec<_>>();
        // The batch reports through the engine's own transfer events; the bridge turns them into
        // one byte total so the caller can show movement while the files are being fetched.
        let bridge = ArtifactDownloadBridge::new(observer);
        let transfer_observer: Arc<dyn TransferObserver> = {
            let bridge = Arc::clone(&bridge);
            Arc::new(move |event: TransferEvent| bridge.observe(&event))
        };
        DownloadBytes::download_many_observed(self, &requests, transfer_observer)
            .into_iter()
            .zip(&requests)
            .map(|(result, request)| {
                result.map_err(|error| ArtifactInstallError::Download {
                    url: request.url.clone(),
                    reason: error.to_string(),
                })
            })
            .collect()
    }
}

/// Turns one artifact batch's transfer events into aggregate byte progress.
struct ArtifactDownloadBridge {
    state: Mutex<ArtifactDownloadBridgeState>,
    observer: Arc<ArtifactProgressObserver>,
}

#[derive(Default)]
struct ArtifactDownloadBridgeState {
    /// Highest byte count seen per task, so retries cannot move the total backwards.
    transferred: BTreeMap<String, u64>,
    totals: BTreeMap<String, u64>,
    active_requests: usize,
}

impl ArtifactDownloadBridge {
    fn new(observer: Arc<ArtifactProgressObserver>) -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(ArtifactDownloadBridgeState::default()),
            observer,
        })
    }

    fn observe(&self, event: &TransferEvent) {
        let Ok(mut state) = self.state.lock() else {
            return;
        };
        let transferred = state.transferred.entry(event.task_id.clone()).or_default();
        *transferred = (*transferred).max(event.transferred_bytes);
        if let Some(total) = event.total_bytes {
            state.totals.insert(event.task_id.clone(), total);
        }
        state.active_requests = event.active_requests;
        let progress = ArtifactDownloadProgress {
            transferred_bytes: state.transferred.values().copied().sum(),
            total_bytes: state.totals.values().copied().sum(),
            active_requests: state.active_requests,
        };
        drop(state);
        (self.observer)(progress);
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

impl SyncConfig {
    pub fn from_path(path: impl AsRef<Path>) -> Result<Self, RunError> {
        let path = path.as_ref();
        let text = fs::read_to_string(path).map_err(|source| RunError::ReadSyncConfig {
            path: path.display().to_string(),
            source,
        })?;
        let config: Self = toml::from_str(&text).map_err(|source| RunError::ParseSyncConfig {
            path: path.display().to_string(),
            source,
        })?;
        config.validate()?;
        Ok(config)
    }

    /// Loads synchronization settings for GUI editing without making the page depend on valid
    /// TOML content. Missing files and malformed fields use defaults; filesystem failures remain
    /// errors. Strict validation remains in [`Self::from_path`] and [`Self::validate`].
    pub fn from_path_for_gui(
        path: impl AsRef<Path>,
        default_include_paths: Vec<String>,
    ) -> Result<Self, RunError> {
        let path = path.as_ref();
        let default = || Self {
            include_paths: default_include_paths.clone(),
        };
        let text = match fs::read_to_string(path) {
            Ok(text) => text,
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(default()),
            Err(source) => {
                return Err(RunError::ReadSyncConfig {
                    path: path.display().to_string(),
                    source,
                });
            }
        };
        let Ok(value) = toml::from_str::<toml::Value>(&text) else {
            return Ok(default());
        };
        let Some(include_paths) = value.get("include_paths").and_then(toml::Value::as_array) else {
            return Ok(default());
        };
        if include_paths.is_empty() {
            return Ok(Self {
                include_paths: Vec::new(),
            });
        }

        let include_paths = include_paths
            .iter()
            .filter_map(toml::Value::as_str)
            .map(str::trim)
            .filter(|path| is_valid_sync_include_path(path))
            .map(str::to_owned)
            .collect::<Vec<_>>();
        if include_paths.is_empty() {
            Ok(default())
        } else {
            Ok(Self { include_paths })
        }
    }

    pub fn validate(&self) -> Result<(), RunError> {
        for path in &self.include_paths {
            let path = path.trim();
            if !is_valid_sync_include_path(path) {
                return Err(RunError::InvalidSyncIncludePath {
                    path: path.to_string(),
                });
            }
        }
        Ok(())
    }
}

impl TestRunConfig {
    /// Loads the local run form without surfacing TOML content errors while the GUI page opens.
    /// Each malformed or missing field retains its documented default, while filesystem failures
    /// remain visible to the caller.
    pub fn from_project_for_gui(project_dir: impl AsRef<Path>) -> Result<Self, ConfigError> {
        let path = Self::path(project_dir);
        let text = match fs::read_to_string(&path) {
            Ok(text) => text,
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self::default());
            }
            Err(source) => {
                return Err(ConfigError::Read {
                    path: path.display().to_string(),
                    source,
                });
            }
        };
        let Ok(value) = toml::from_str::<toml::Value>(&text) else {
            return Ok(Self::default());
        };
        let Some(table) = value.as_table() else {
            return Ok(Self::default());
        };

        let mut config = Self::default();
        if let Some(value) = table.get("player_name").and_then(toml::Value::as_str) {
            config.player_name = value.to_string();
        }
        if let Some(value) = table
            .get("download_source")
            .and_then(toml::Value::as_str)
            .and_then(parse_gui_download_source)
        {
            config.download_source = value;
        }
        if let Some(value) = table
            .get("sync_interval_seconds")
            .and_then(toml::Value::as_integer)
            .and_then(|value| u64::try_from(value).ok())
            .filter(|value| *value > 0)
        {
            config.sync_interval_seconds = value;
        }
        if let Some(value) = table
            .get("debug_extra_jvm_args")
            .and_then(toml::Value::as_array)
            .and_then(|values| {
                values
                    .iter()
                    .map(toml::Value::as_str)
                    .map(|value| value.map(str::to_owned))
                    .collect::<Option<Vec<_>>>()
            })
        {
            config.debug_extra_jvm_args = value;
        }
        if let Some(value) = table.get("java_path").and_then(toml::Value::as_str) {
            config.java_path = value.to_string();
        }
        Ok(config)
    }
}

fn parse_gui_download_source(value: &str) -> Option<DownloadSource> {
    match value {
        "auto" => Some(DownloadSource::Auto),
        "official" => Some(DownloadSource::Official),
        "bmclapi" => Some(DownloadSource::Bmclapi),
        _ => None,
    }
}

fn is_valid_sync_include_path(path: &str) -> bool {
    !path.is_empty()
        && !Path::new(path).is_absolute()
        && Path::new(path)
            .components()
            .all(|component| matches!(component, std::path::Component::Normal(_)))
        && !is_generated_or_internal_path(path)
}

pub fn sync_config_for_project(project_dir: &Path, side: RunSide) -> Result<SyncConfig, RunError> {
    let path = project_dir
        .join("run")
        .join(side.dir_name())
        .join(".mcmpu-sync.toml");
    if path.exists() {
        return SyncConfig::from_path(path);
    }
    Ok(SyncConfig {
        include_paths: default_sync_include_paths(project_dir, side)?,
    })
}

/// Loads the side-specific synchronization form for the GUI. Missing or malformed configuration
/// uses editable defaults, while failures reading the configuration or default source directories
/// remain visible. Strict command execution continues to use [`sync_config_for_project`].
pub fn sync_config_for_project_for_gui(
    project_dir: &Path,
    side: RunSide,
) -> Result<SyncConfig, RunError> {
    let defaults = default_sync_include_paths(project_dir, side)?;
    let path = project_dir
        .join("run")
        .join(side.dir_name())
        .join(".mcmpu-sync.toml");
    SyncConfig::from_path_for_gui(path, defaults)
}

pub fn write_sync_config_for_project(
    project_dir: &Path,
    side: RunSide,
    config: &SyncConfig,
) -> Result<(), RunError> {
    config.validate()?;
    let run_dir = project_dir.join("run").join(side.dir_name());
    create_dir(&run_dir)?;
    let path = run_dir.join(".mcmpu-sync.toml");
    let text = toml::to_string_pretty(config)?;
    fs::write(&path, text).map_err(|source| RunError::WriteFile {
        path: path.display().to_string(),
        source,
    })
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
    let (mut config, resolved) = resolve_project_for_runtime_with_api(project_dir, api)?;
    config.test_run = TestRunConfig::from_project(project_dir)?;
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
    run_project_with_api_key_and_progress_and_stop_signal(
        project_dir,
        api_key,
        proxy,
        side,
        &NeverStop,
        &NeverConsole,
        progress,
    )
}

/// Prepares, launches, and supervises a local development run with caller-provided cancellation.
///
/// This is the CLI-facing variant used to connect Ctrl+C to the same lifecycle that handles GUI
/// cancellation: process shutdown and pending-sync scanning both occur before the function
/// returns.
pub fn run_project_with_api_key_and_progress_and_stop_signal<F, S>(
    project_dir: impl AsRef<Path>,
    api_key: String,
    proxy: Option<&str>,
    side: RunSide,
    stop_signal: &S,
    console: &dyn ConsoleInput,
    progress: &mut F,
) -> Result<RunLaunchPlan, RunError>
where
    F: FnMut(&RunProgress),
    S: StopSignal + ?Sized,
{
    let project_dir = project_dir.as_ref();
    let run_dir = project_dir.join("run").join(side.dir_name());
    let lifecycle = (|| {
        let network = Arc::new(NetworkEngine::new(crate::net::NetworkConfig {
            proxy: proxy
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_owned),
            ..crate::net::NetworkConfig::default()
        })?);
        let installer = RuntimeProfileLocatorImpl::with_network_engine(
            project_dir.join(".mcmpu/cache/runtime"),
            proxy,
            Arc::clone(&network),
        )?;
        let api = CurseForgeClient::with_proxy(api_key.clone(), proxy).map_err(BuildError::from)?;
        let plan = prepare_run_directory_with_api_and_progress(project_dir, &api, side, progress)?;
        let downloader = HttpDownloader::with_network_engine(Arc::clone(&network), Some(&api_key))?;
        verify_or_download_run_files_with_progress(
            &plan.run_dir,
            &plan.files,
            &downloader,
            progress,
        )?;
        let mut config = Config::from_path(project_dir.join("config.toml"))?;
        config.test_run = TestRunConfig::from_project(project_dir)?;
        progress(&RunProgress::ResolvingJava {
            major: config.java.version_major,
        });
        let java = resolve_or_download_java_executable(
            project_dir,
            &config.test_run,
            config.java.version_major,
            &SystemJavaLocator,
            &downloader,
        )?;
        progress(&RunProgress::JavaResolved { path: java.clone() });
        if side == RunSide::Server {
            let mut args = plan.java_args.clone();
            let server_arguments = installer.prepare_server_arguments(
                &plan.run_dir,
                &config,
                &java,
                &mut |event| forward_runtime_install_progress(side, event, progress),
            )?;
            args.extend(server_arguments);
            let command = LaunchCommand {
                side,
                working_dir: plan.run_dir.clone(),
                executable: java.clone(),
                args,
            };
            reapply_overlay_layers(project_dir, &plan.run_dir, side, progress)?;
            let changed = ensure_server_run_configuration(&plan.run_dir)?;
            if !changed.is_empty() {
                progress(&RunProgress::ServerRunConfigurationWritten { changed });
            }
            progress(&RunProgress::Launching {
                command: format!(
                    "{} {}",
                    command.executable.display(),
                    command.args.join(" ")
                ),
            });
            run_process_with_stop_signal_and_events(
                &command,
                &SystemProcessRunner,
                stop_signal,
                console,
                &mut |event| forward_process_output_progress(side, event, progress),
            )?;
            return Ok(plan);
        }
        let profile_id =
            installer.prepare_client_profile(&plan.run_dir, &config, &java, &mut |event| {
                forward_runtime_install_progress(side, event, progress)
            })?;
        let environment = current_launch_environment()?;
        let descriptor =
            crate::project::launcher::LoaderLaunchResolver::new(profile_source(&plan.run_dir))
                .resolve_client(config.pack.loader.clone(), &profile_id, &environment)?;
        ArtifactInstallerImpl::new(&downloader).install_with_progress(
            &descriptor,
            &plan.run_dir,
            &project_dir.join(".mcmpu/cache/runtime"),
            &mut |event| forward_artifact_install_progress(side, event, progress),
        )?;
        let command = plan_launch_command_from_descriptor(&plan, &descriptor, &java)?;
        reapply_overlay_layers(project_dir, &plan.run_dir, side, progress)?;
        progress(&RunProgress::Launching {
            command: format!(
                "{} {}",
                command.executable.display(),
                command.args.join(" ")
            ),
        });
        run_process_with_stop_signal_and_events(
            &command,
            &SystemProcessRunner,
            stop_signal,
            console,
            &mut |event| forward_process_output_progress(side, event, progress),
        )?;
        Ok(plan)
    })();

    finalize_pending_sync_after_run_with_progress(project_dir, &run_dir, side, lifecycle, progress)
}

/// Completes a run attempt by scanning development-file changes whenever a valid source snapshot
/// exists. The run error remains primary; a simultaneous scan failure is attached explicitly.
pub fn finalize_pending_sync_after_run<T>(
    project_dir: &Path,
    run_dir: &Path,
    side: RunSide,
    lifecycle: Result<T, RunError>,
) -> Result<T, RunError> {
    finalize_pending_sync_after_run_with_progress(
        project_dir,
        run_dir,
        side,
        lifecycle,
        &mut |_| {},
    )
}

fn finalize_pending_sync_after_run_with_progress<T>(
    project_dir: &Path,
    run_dir: &Path,
    side: RunSide,
    lifecycle: Result<T, RunError>,
    progress: &mut impl FnMut(&RunProgress),
) -> Result<T, RunError> {
    if !run_dir.join(RUN_SOURCES_FILE).is_file() {
        return lifecycle;
    }

    let pending_path = pending_sync_path(project_dir, side);
    let pending = scan_pending_sync(project_dir, run_dir, side);
    match &pending {
        Ok(manifest) => progress(&RunProgress::PendingSyncWritten {
            path: manifest.path.clone(),
            entries: manifest.entries.len(),
        }),
        Err(error) => progress(&RunProgress::PendingSyncFailed {
            path: pending_path,
            error: error.to_string(),
        }),
    }

    match (lifecycle, pending) {
        (Ok(value), Ok(_)) => Ok(value),
        (Err(lifecycle), Ok(_)) => Err(lifecycle),
        (Ok(_), Err(pending_sync)) => Err(pending_sync),
        (Err(lifecycle), Err(pending_sync)) => Err(RunError::LifecycleAndPendingSync {
            lifecycle: Box::new(lifecycle),
            pending_sync: Box::new(pending_sync),
        }),
    }
}

fn forward_runtime_install_progress<F>(
    side: RunSide,
    event: &RuntimeInstallProgress,
    progress: &mut F,
) where
    F: FnMut(&RunProgress),
{
    let event = match event {
        RuntimeInstallProgress::DownloadingInstaller { url, target } => {
            RunProgress::DownloadingLoaderInstaller {
                side,
                url: url.clone(),
                target: target.clone(),
            }
        }
        RuntimeInstallProgress::InstallerDownloaded { target } => {
            RunProgress::LoaderInstallerDownloaded {
                side,
                target: target.clone(),
            }
        }
        RuntimeInstallProgress::RunningInstaller { loader } => {
            RunProgress::RunningLoaderInstaller {
                side,
                loader: loader.clone(),
            }
        }
        RuntimeInstallProgress::InstallerOutput { stream, line } => {
            RunProgress::LoaderInstallerOutput {
                side,
                stream: *stream,
                line: line.clone(),
            }
        }
        RuntimeInstallProgress::InstallingLibraries { total } => {
            RunProgress::InstallingLoaderLibraries {
                side,
                total: *total,
            }
        }
        RuntimeInstallProgress::LibraryReady {
            completed,
            total,
            path,
        } => RunProgress::LoaderLibraryReady {
            side,
            completed: *completed,
            total: *total,
            path: path.clone(),
        },
    };
    progress(&event);
}

fn forward_artifact_install_progress<F>(
    side: RunSide,
    event: &ArtifactInstallProgress,
    progress: &mut F,
) where
    F: FnMut(&RunProgress),
{
    let event = match event {
        ArtifactInstallProgress::Starting { total } => RunProgress::PreparingRuntimeArtifacts {
            side,
            total: *total,
        },
        ArtifactInstallProgress::TotalUpdated { total } => {
            RunProgress::RuntimeArtifactTotalUpdated {
                side,
                total: *total,
            }
        }
        ArtifactInstallProgress::ArtifactReady {
            completed,
            total,
            path,
            source,
        } => RunProgress::RuntimeArtifactReady {
            side,
            completed: *completed,
            total: *total,
            path: path.clone(),
            source: *source,
        },
        ArtifactInstallProgress::Downloading {
            transferred_bytes,
            total_bytes,
            active_requests,
        } => RunProgress::RuntimeArtifactDownloadProgress {
            side,
            transferred_bytes: *transferred_bytes,
            total_bytes: *total_bytes,
            active_requests: *active_requests,
        },
    };
    progress(&event);
}

fn forward_process_output_progress<F>(side: RunSide, event: &RunEvent, progress: &mut F)
where
    F: FnMut(&RunProgress),
{
    if let RunEvent::ProcessOutput { stream, line, .. } = event {
        progress(&RunProgress::MinecraftOutput {
            side,
            stream: *stream,
            line: line.clone(),
        });
    }
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
    let source_manifest_path = run_dir.join(RUN_SOURCES_FILE);
    if source_manifest_path.exists() {
        fs::remove_file(&source_manifest_path).map_err(|source| RunError::RemoveFile {
            path: source_manifest_path.display().to_string(),
            source,
        })?;
    }
    progress(&RunProgress::RebuildingRunDirectory {
        side,
        run_dir: run_dir.clone(),
    });
    ensure_generated_dirs(&run_dir)?;
    progress(&RunProgress::CopyingOverrides { side });
    let mut sources = BTreeMap::new();
    copy_override_layers(project_dir, &run_dir, side, &mut sources)?;
    progress(&RunProgress::CopyingDebug { side });
    copy_debug_layers(project_dir, &run_dir, side, &mut sources)?;
    progress(&RunProgress::EnsuringSyncConfig {
        side,
        path: run_dir.join(".mcmpu-sync.toml"),
    });
    ensure_sync_config(project_dir, &run_dir, side)?;
    // Everything this run wants in the generated directories: the overlay copies plus the files
    // the manifests list. Whatever else sits in their top level is left over from an earlier run.
    let mut managed: BTreeSet<String> = sources.keys().cloned().collect();
    write_source_manifest(&run_dir, side, sources)?;
    let files = runtime_files(project_dir, resolved, side)?;
    managed.extend(files.iter().map(|file| file.path.clone()));
    write_run_manifest(&run_dir, &files)?;
    prune_unmanaged_generated_files(&run_dir, &managed)?;
    progress(&RunProgress::RunManifestWritten {
        file_count: files.len(),
    });
    Ok(RunLaunchPlan {
        side,
        run_dir,
        java_args: java_args_for_side(config, side)?,
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
    let gradle_home = gradle_user_home();
    verify_or_download_run_files_at(run_dir, files, downloader, gradle_home.as_deref(), progress)
}

/// Verifies run files against an explicit Gradle user home instead of the ambient one.
fn verify_or_download_run_files_at(
    run_dir: &Path,
    files: &[RunFile],
    downloader: &impl DownloadBytes,
    gradle_home: Option<&Path>,
    progress: &mut impl FnMut(&RunProgress),
) -> Result<(), RunError> {
    progress(&RunProgress::DownloadingFiles { total: files.len() });
    progress(&RunProgress::VerifyingFiles { total: files.len() });
    let mut lock_paths = Vec::new();
    let mut initial_target_hits = Vec::with_capacity(files.len());
    let mut last_verification_progress = Instant::now();
    for (index, file) in files.iter().enumerate() {
        let target = run_dir.join(&file.path);
        let target_hit = is_valid_file(&target, file.size, &file.sha1)?;
        initial_target_hits.push(target_hit);
        if index + 1 == files.len()
            || (index + 1) % 16 == 0
            || last_verification_progress.elapsed() >= RUN_DOWNLOAD_PROGRESS_INTERVAL
        {
            progress(&RunProgress::FileVerificationProgress {
                checked: index + 1,
                total: files.len(),
            });
            last_verification_progress = Instant::now();
        }
        if target_hit {
            continue;
        }
        let cache_lock_path = file.cache_path.with_file_name(format!(
            ".{}.lock",
            file.cache_path
                .file_name()
                .map(|name| name.to_string_lossy())
                .unwrap_or_default()
        ));
        lock_paths.push(cache_lock_path);
    }
    lock_paths.sort();
    lock_paths.dedup();
    let mut cache_locks = Vec::with_capacity(lock_paths.len());
    for path in lock_paths {
        cache_locks.push(CacheFileLock::acquire(&path)?);
    }

    let mut requests = Vec::new();
    let mut locally_ready = Vec::new();
    let mut reused = 0_usize;
    let mut cached = 0_usize;
    for (file, initial_target_hit) in files.iter().zip(&initial_target_hits) {
        if *initial_target_hit {
            reused += 1;
            locally_ready.push((file.path.clone(), RunFileReadySource::Reused));
            continue;
        }
        let target = run_dir.join(&file.path);
        recover_file_backup(&file.cache_path)?;
        cleanup_orphaned_parts(&file.cache_path)?;
        recover_file_backup(&target)?;
        cleanup_orphaned_parts(&target)?;
        if is_valid_file(&target, file.size, &file.sha1)? {
            reused += 1;
            locally_ready.push((file.path.clone(), RunFileReadySource::Reused));
            continue;
        }
        if is_valid_file(&file.cache_path, file.size, &file.sha1)? {
            publish_run_file_from_cache(run_dir, file)?;
            cached += 1;
            locally_ready.push((file.path.clone(), RunFileReadySource::Cache));
            continue;
        }
        if let Some(gradle_path) = find_gradle_cached_run_file_at(file, gradle_home) {
            // The project cache keeps the Gradle file's timestamp, so an already imported copy is
            // left alone instead of being written out again on every start.
            if !target_matches_source(&gradle_path, &file.cache_path) {
                copy_file_atomically(&gradle_path, &file.cache_path)?;
            }
            publish_run_file_from_cache(run_dir, file)?;
            cached += 1;
            locally_ready.push((file.path.clone(), RunFileReadySource::Gradle));
            continue;
        }
        requests.push(RunDownloadRequest {
            task_id: file.path.clone(),
            url: file.url.clone(),
            fallback_urls: curseforge_artifact_fallbacks(&file.url),
            target: file.cache_path.clone(),
            expected_size: Some(file.size),
            expected_sha1: Some(file.sha1.clone()),
            expected_sha256: None,
        });
    }

    let total_download_bytes = requests
        .iter()
        .filter_map(|request| request.expected_size)
        .fold(0_u64, u64::saturating_add);
    progress(&RunProgress::RunFilesChecked {
        reused,
        cached,
        downloading: requests.len(),
        download_bytes: total_download_bytes,
    });

    let mut ready_paths = BTreeSet::new();
    let mut ready_count = 0_usize;
    for (path, source) in locally_ready {
        ready_paths.insert(path.clone());
        ready_count += 1;
        progress(&RunProgress::FileReady {
            completed: ready_count,
            total: files.len(),
            path,
            source,
        });
    }

    let files_by_task = files
        .iter()
        .map(|file| (file.path.clone(), file))
        .collect::<BTreeMap<_, _>>();
    let mut aggregate = RunDownloadAggregate::new(&requests);
    if !requests.is_empty() {
        progress(&RunProgress::DownloadBatchStarted {
            total: requests.len(),
            total_bytes: total_download_bytes,
        });
    }
    let results = download_many_with_live_events(downloader, &requests, &mut |event| {
        let task_id = event.task_id.clone();
        let update = aggregate.observe(&event)?;
        if update.started {
            progress(&RunProgress::FileDownloadStarted {
                started: aggregate.started_count(),
                total: requests.len(),
                path: task_id.clone(),
            });
        }
        if update.completed && !ready_paths.contains(&task_id) {
            let file =
                files_by_task
                    .get(&task_id)
                    .ok_or_else(|| RunError::InvalidDownloadBatch {
                        reason: format!("download progress referenced unknown task '{task_id}'"),
                    })?;
            publish_run_file_from_cache(run_dir, file)?;
            ready_paths.insert(task_id.clone());
            ready_count += 1;
            progress(&RunProgress::FileReady {
                completed: ready_count,
                total: files.len(),
                path: task_id,
                source: RunFileReadySource::Downloaded,
            });
        }
        if aggregate.should_emit_progress(update.completed) {
            progress(&aggregate.progress_event());
        }
        Ok(())
    })?;
    if results.len() != requests.len() {
        return Err(RunError::InvalidDownloadBatch {
            reason: format!(
                "downloader returned {} result(s) for {} request(s)",
                results.len(),
                requests.len()
            ),
        });
    }
    for result in results {
        result?;
    }
    if !requests.is_empty() && !aggregate.just_emitted_progress() {
        progress(&aggregate.progress_event());
    }

    for file in files {
        let target = run_dir.join(&file.path);
        // A target already carrying this cache file's size and timestamp is this very copy, so
        // nothing is read or written for it; anything else is compared by content first.
        if !run_target_is_current(&file.cache_path, &target, file.size, &file.sha1)? {
            if !is_valid_file(&file.cache_path, file.size, &file.sha1)? {
                return Err(RunError::Sha1Mismatch {
                    path: file.cache_path.display().to_string(),
                    expected: file.sha1.clone(),
                    actual: file_fingerprint(&file.cache_path)?.sha1,
                });
            }
            if let Some(parent) = target.parent() {
                create_dir(parent)?;
            }
            copy_file_atomically(&file.cache_path, &target)?;
            if !run_target_is_current(&file.cache_path, &target, file.size, &file.sha1)? {
                return Err(RunError::Sha1Mismatch {
                    path: target.display().to_string(),
                    expected: file.sha1.clone(),
                    actual: file_fingerprint(&target)?.sha1,
                });
            }
        }
        if ready_paths.insert(file.path.clone()) {
            ready_count += 1;
            progress(&RunProgress::FileReady {
                completed: ready_count,
                total: files.len(),
                path: file.path.clone(),
                source: if requests.iter().any(|request| request.task_id == file.path) {
                    RunFileReadySource::Downloaded
                } else {
                    RunFileReadySource::Cache
                },
            });
        }
    }
    drop(cache_locks);
    Ok(())
}

fn publish_run_file_from_cache(run_dir: &Path, file: &RunFile) -> Result<(), RunError> {
    if !is_valid_file(&file.cache_path, file.size, &file.sha1)? {
        return Err(RunError::Sha1Mismatch {
            path: file.cache_path.display().to_string(),
            expected: file.sha1.clone(),
            actual: file_fingerprint(&file.cache_path)?.sha1,
        });
    }
    let target = run_dir.join(&file.path);
    // A target that already is this cache file is left alone: rewriting it would move a
    // timestamp that mods key their caches on, and reading it back would cost a full scan.
    if run_target_is_current(&file.cache_path, &target, file.size, &file.sha1)? {
        return Ok(());
    }
    if let Some(parent) = target.parent() {
        create_dir(parent)?;
    }
    copy_file_atomically(&file.cache_path, &target)?;
    if !run_target_is_current(&file.cache_path, &target, file.size, &file.sha1)? {
        return Err(RunError::Sha1Mismatch {
            path: target.display().to_string(),
            expected: file.sha1.clone(),
            actual: file_fingerprint(&target)?.sha1,
        });
    }
    Ok(())
}

/// Reports whether `target` already holds the right content for the file described by
/// `size` and `sha1`.
///
/// Size and modification time are the fast path: every copy mcmpu writes takes over the source's
/// timestamp, so a match means the file came from this source and need not be read at all. A file
/// that fails the fast path is still compared by content before anything is written again.
fn run_target_is_current(
    source: &Path,
    target: &Path,
    size: u64,
    sha1: &str,
) -> Result<bool, RunError> {
    if target_matches_source(source, target) {
        return Ok(true);
    }
    is_valid_file(target, size, sha1)
}

#[derive(Debug, Clone, Copy)]
struct RunDownloadUpdate {
    started: bool,
    completed: bool,
}

struct RunDownloadAggregate {
    tasks: BTreeMap<String, (u64, u64)>,
    started: BTreeSet<String>,
    completed: BTreeSet<String>,
    started_at: Instant,
    last_progress_at: Instant,
    last_progress_completed: usize,
    progress_emitted: bool,
    active_requests: usize,
}

impl RunDownloadAggregate {
    fn new(requests: &[RunDownloadRequest]) -> Self {
        let tasks = requests
            .iter()
            .map(|request| {
                (
                    request.task_id.clone(),
                    (0, request.expected_size.unwrap_or(0)),
                )
            })
            .collect();
        let now = Instant::now();
        Self {
            tasks,
            started: BTreeSet::new(),
            completed: BTreeSet::new(),
            started_at: now,
            last_progress_at: now,
            last_progress_completed: 0,
            progress_emitted: false,
            active_requests: 0,
        }
    }

    fn observe(&mut self, event: &TransferEvent) -> Result<RunDownloadUpdate, RunError> {
        let Some((transferred, total)) = self.tasks.get_mut(&event.task_id) else {
            return Err(RunError::InvalidDownloadBatch {
                reason: format!(
                    "downloader emitted progress for unknown task '{}'",
                    event.task_id
                ),
            });
        };
        *transferred = (*transferred).max(event.transferred_bytes);
        if let Some(event_total) = event.total_bytes {
            *total = event_total;
        }
        self.active_requests = event.active_requests;
        let active = matches!(
            event.phase,
            TransferPhase::Probing
                | TransferPhase::Single
                | TransferPhase::Segmented
                | TransferPhase::Retrying
                | TransferPhase::Verifying
        );
        let started = active && self.started.insert(event.task_id.clone());
        let completed =
            event.phase == TransferPhase::Completed && self.completed.insert(event.task_id.clone());
        Ok(RunDownloadUpdate { started, completed })
    }

    fn started_count(&self) -> usize {
        self.started.len()
    }

    fn should_emit_progress(&mut self, completed: bool) -> bool {
        let now = Instant::now();
        if !completed && now.duration_since(self.last_progress_at) < RUN_DOWNLOAD_PROGRESS_INTERVAL
        {
            return false;
        }
        if completed
            && self.completed.len() != self.tasks.len()
            && now.duration_since(self.last_progress_at) < RUN_DOWNLOAD_PROGRESS_INTERVAL
        {
            return false;
        }
        self.last_progress_at = now;
        self.last_progress_completed = self.completed.len();
        self.progress_emitted = true;
        true
    }

    fn just_emitted_progress(&self) -> bool {
        self.progress_emitted && self.last_progress_completed == self.completed.len()
    }

    fn progress_event(&self) -> RunProgress {
        let transferred_bytes = self
            .tasks
            .values()
            .map(|(transferred, _)| *transferred)
            .fold(0_u64, u64::saturating_add);
        let total_bytes = self
            .tasks
            .values()
            .map(|(_, total)| *total)
            .fold(0_u64, u64::saturating_add);
        let elapsed = self.started_at.elapsed().as_secs_f64();
        RunProgress::DownloadProgress {
            completed: self.completed.len(),
            total: self.tasks.len(),
            transferred_bytes,
            total_bytes,
            bytes_per_second: if elapsed > 0.0 {
                (transferred_bytes as f64 / elapsed) as u64
            } else {
                0
            },
            active_requests: self.active_requests,
        }
    }
}

fn download_many_with_live_events(
    downloader: &impl DownloadBytes,
    requests: &[RunDownloadRequest],
    on_event: &mut impl FnMut(TransferEvent) -> Result<(), RunError>,
) -> Result<Vec<Result<(), RunError>>, RunError> {
    if requests.is_empty() {
        return Ok(Vec::new());
    }
    thread::scope(|scope| {
        let (event_sender, event_receiver) = mpsc::sync_channel(RUN_DOWNLOAD_EVENT_QUEUE);
        let observer: Arc<dyn TransferObserver> = Arc::new(move |event: TransferEvent| {
            let _ = event_sender.send(event);
        });
        let worker = scope.spawn(move || downloader.download_many_observed(requests, observer));
        let mut callback_error = None;
        while !worker.is_finished() {
            match event_receiver.recv_timeout(Duration::from_millis(50)) {
                Ok(event) if callback_error.is_none() => {
                    if let Err(error) = on_event(event) {
                        callback_error = Some(error);
                    }
                }
                Ok(_) | Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
        while let Ok(event) = event_receiver.try_recv() {
            if callback_error.is_none()
                && let Err(error) = on_event(event)
            {
                callback_error = Some(error);
            }
        }
        let results = worker.join().map_err(|_| RunError::InvalidDownloadBatch {
            reason: "download worker panicked before returning batch results".to_string(),
        })?;
        if let Some(error) = callback_error {
            return Err(error);
        }
        Ok(results)
    })
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
        if is_generated_or_internal_path(&source.path) {
            let run_path = run_dir.join(&source.path);
            let changed = match file_fingerprint(&run_path) {
                Ok(current) => current.sha1 != source.sha1 || current.size != source.size,
                Err(RunError::ReadFile { source, .. })
                    if source.kind() == std::io::ErrorKind::NotFound =>
                {
                    true
                }
                Err(error) => return Err(error),
            };
            if changed {
                ignored_generated += 1;
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
    for entry in &pending.entries {
        validate_pending_sync_entry(entry)?;
    }

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
                // The run copy carries the moment the mod wrote it, which is what the overlay
                // source should say as well.
                copy_file_keeping_mtime(&source, &target)?;
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

fn validate_pending_sync_entry(entry: &PendingSyncEntry) -> Result<(), RunError> {
    validate_portable_relative_path(&entry.path).map_err(|error| {
        RunError::InvalidPendingSyncEntry {
            path: entry.path.clone(),
            reason: error.to_string(),
        }
    })?;
    if is_generated_or_internal_path(&entry.path) {
        return Err(RunError::InvalidPendingSyncEntry {
            path: entry.path.clone(),
            reason: "generated and internal run paths cannot be synchronized".to_string(),
        });
    }

    match (entry.change, entry.source) {
        (PendingSyncChange::Added, None) => Ok(()),
        (PendingSyncChange::Added, Some(_)) => Err(RunError::InvalidPendingSyncEntry {
            path: entry.path.clone(),
            reason: "an added file cannot claim an existing source layer".to_string(),
        }),
        (PendingSyncChange::Modified | PendingSyncChange::Deleted, Some(source))
            if source.is_debug() =>
        {
            Err(RunError::InvalidPendingSyncEntry {
                path: entry.path.clone(),
                reason: "debug files are runtime-only and cannot be written to overrides"
                    .to_string(),
            })
        }
        (PendingSyncChange::Modified | PendingSyncChange::Deleted, Some(source)) => {
            if source.target() == Some(entry.target) {
                Ok(())
            } else {
                Err(RunError::InvalidPendingSyncEntry {
                    path: entry.path.clone(),
                    reason: "the target does not match the original overrides layer".to_string(),
                })
            }
        }
        (PendingSyncChange::Modified | PendingSyncChange::Deleted, None) => {
            Err(RunError::InvalidPendingSyncEntry {
                path: entry.path.clone(),
                reason: "modified and deleted files must identify their overrides source layer"
                    .to_string(),
            })
        }
    }
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

pub fn curseforge_cdn_artifact_url(file: &CfFile) -> Result<String, RunError> {
    if file.file_name.trim().is_empty() {
        return Err(RunError::EmptyCurseForgeFileName {
            project_id: file.mod_id,
            file_id: file.id,
        });
    }
    let prefix = file.id / 1_000;
    let suffix = file.id % 1_000;
    Ok(format!(
        "https://edge.forgecdn.net/files/{prefix}/{suffix}/{}",
        encode_url_path_segment(&file.file_name)
    ))
}

/// Name one run file should use inside its `mods/`, `resourcepacks/`, or `shaderpacks/` folder.
///
/// Only mods are renamed to `<sort prefix><slug>-<project id>-<file id>.<ext>`: Forge loads every
/// jar in `mods/` and ignores the name, while the identity makes upgrades replace the old file
/// and lets the load-order prefix survive.
///
/// Resource packs and shader packs are referenced *by name* from the player's game options, so
/// their original file name is part of the configuration that ships in the built archive.
/// Renaming them would make the pack list in `options.txt` point at a file that no longer exists.
pub fn run_file_name(dependency_type: &DependencyType, slug: &str, file: &CfFile) -> String {
    if *dependency_type != DependencyType::Mod {
        return file.file_name.clone();
    }
    let extension = extension_of(&file.file_name);
    format!(
        "{}{slug}-{}-{}.{}",
        sort_prefix(&file.file_name),
        file.mod_id,
        file.id,
        extension
    )
}

fn encode_url_path_segment(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len());
    for byte in value.as_bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            encoded.push(char::from(*byte));
        } else {
            use std::fmt::Write as _;
            write!(&mut encoded, "%{byte:02X}").expect("writing into String cannot fail");
        }
    }
    encoded
}

fn is_curseforge_cdn_url(url: &str) -> bool {
    reqwest::Url::parse(url)
        .ok()
        .and_then(|parsed| parsed.host_str().map(str::to_ascii_lowercase))
        .is_some_and(|host| {
            matches!(
                host.as_str(),
                "edge.forgecdn.net" | "mediafilez.forgecdn.net"
            )
        })
}

/// Returns the local Gradle user home without invoking Gradle itself.
fn gradle_user_home() -> Option<PathBuf> {
    env::var_os("GRADLE_USER_HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            env::var_os("USERPROFILE")
                .or_else(|| env::var_os("HOME"))
                .map(|home| PathBuf::from(home).join(".gradle"))
        })
}

/// Parses the generated run filename into the CurseMaven module and artifact name.
///
/// Runtime names are `<sort-prefix><slug>-<project-id>-<file-id>.<extension>`. The sort prefix
/// is not part of the Gradle artifact name, so candidates are matched both with and without it.
fn gradle_curseforge_coordinates(path: &str) -> Option<(String, String, String)> {
    let file_name = Path::new(path).file_name()?.to_str()?;
    let file_name =
        file_name.trim_start_matches(|character: char| !character.is_ascii_alphanumeric());
    let extension = Path::new(file_name).extension()?.to_str()?.to_string();
    let stem = Path::new(file_name).file_stem()?.to_str()?;
    let (prefix, file_id) = stem.rsplit_once('-')?;
    let (slug, project_id) = prefix.rsplit_once('-')?;
    if slug.is_empty()
        || project_id.is_empty()
        || file_id.is_empty()
        || !project_id
            .chars()
            .all(|character| character.is_ascii_digit())
        || !file_id.chars().all(|character| character.is_ascii_digit())
    {
        return None;
    }
    Some((
        format!("{slug}-{project_id}"),
        file_id.to_string(),
        extension,
    ))
}

/// Finds a verified original CurseMaven artifact in Gradle's `files-2.1` cache.
///
/// The Gradle hash directory only isolates candidate copies; `size`/`sha1` still decide whether a
/// candidate equals the requested file, so an unrelated or damaged cache entry is never reused.
fn find_gradle_cached_run_file_at(file: &RunFile, gradle_home: Option<&Path>) -> Option<PathBuf> {
    let gradle_home = gradle_home?;
    let (module, file_id, extension) = gradle_curseforge_coordinates(&file.path)?;
    let expected_name = format!("{module}-{file_id}.{extension}");
    let root = gradle_home
        .join("caches")
        .join("modules-2")
        .join("files-2.1")
        .join("curse.maven")
        .join(&module)
        .join(&file_id);
    let mut candidates = Vec::new();
    let entries = fs::read_dir(root).ok()?;
    for hash_entry in entries.flatten() {
        let hash_dir = hash_entry.path();
        if !hash_entry.file_type().ok()?.is_dir() {
            continue;
        }
        for artifact_entry in fs::read_dir(hash_dir).ok()?.flatten() {
            let path = artifact_entry.path();
            if !artifact_entry.file_type().ok()?.is_file()
                || path.file_name().and_then(|value| value.to_str()) != Some(expected_name.as_str())
            {
                continue;
            }
            if is_valid_file(&path, file.size, &file.sha1).ok() == Some(true) {
                candidates.push(path);
            }
        }
    }
    candidates.sort();
    candidates.into_iter().next()
}

/// Sets `key=value` in a Minecraft properties file, returning `None` when it is already set.
///
/// Comments and unrelated keys are preserved verbatim, so pack-provided server settings survive.
fn with_property(content: &str, key: &str, value: &str) -> Option<String> {
    let mut lines = Vec::new();
    let mut found = false;
    let mut changed = false;
    for line in content.lines() {
        let trimmed = line.trim_start();
        let is_comment = trimmed.starts_with('#') || trimmed.starts_with('!');
        if !is_comment
            && let Some((name, existing)) = trimmed.split_once('=')
            && name.trim().eq_ignore_ascii_case(key)
        {
            found = true;
            if existing.trim().eq_ignore_ascii_case(value) {
                lines.push(line.to_string());
            } else {
                lines.push(format!("{key}={value}"));
                changed = true;
            }
            continue;
        }
        lines.push(line.to_string());
    }
    if !found {
        lines.push(format!("{key}={value}"));
        changed = true;
    }
    if !changed {
        return None;
    }
    let mut updated = lines.join("\n");
    updated.push('\n');
    Some(updated)
}

/// Prepares the files a dedicated server refuses to start without.
///
/// `runServer` starts a client that authenticates with a fixed offline UUID and token, so the
/// server has to accept offline players. Only the `eula` and `online-mode` keys are controlled;
/// everything else in `server.properties` is left as the pack or a previous run wrote it.
fn ensure_server_run_configuration(run_dir: &Path) -> Result<Vec<String>, RunError> {
    let mut changed = Vec::new();
    for (name, key, value, header) in [
        (EULA_FILE, "eula", "true", EULA_FILE_HEADER),
        (SERVER_PROPERTIES_FILE, "online-mode", "false", ""),
    ] {
        let path = run_dir.join(name);
        let existing = match fs::read_to_string(&path) {
            Ok(text) => text,
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => {
                if header.is_empty() {
                    String::new()
                } else {
                    format!("{header}\n")
                }
            }
            Err(source) => {
                return Err(RunError::ReadFile {
                    path: path.display().to_string(),
                    source,
                });
            }
        };
        let Some(updated) = with_property(&existing, key, value) else {
            continue;
        };
        write_bytes_atomically(&path, updated.as_bytes())?;
        changed.push(name.to_string());
    }
    Ok(changed)
}

fn curseforge_artifact_fallbacks(url: &str) -> Vec<String> {
    let Ok(mut parsed) = reqwest::Url::parse(url) else {
        return Vec::new();
    };
    let Some(host) = parsed.host_str().map(str::to_ascii_lowercase) else {
        return Vec::new();
    };
    let fallback_host = match host.as_str() {
        "edge.forgecdn.net" => "mediafilez.forgecdn.net",
        "mediafilez.forgecdn.net" => "edge.forgecdn.net",
        _ => return Vec::new(),
    };
    if parsed.set_host(Some(fallback_host)).is_err() {
        return Vec::new();
    }
    vec![parsed.to_string()]
}

fn build_network_artifact_request(
    request: &RunDownloadRequest,
    sensitive_headers: &SensitiveHeaders,
) -> Result<ArtifactRequest, NetworkError> {
    let mut builder = ArtifactRequest::builder(&request.task_id, &request.target, &request.url)
        .sensitive_headers(sensitive_headers.clone());
    for url in &request.fallback_urls {
        builder = builder.candidate_url(url);
    }
    if let Some(size) = request.expected_size {
        builder = builder.expected_size(size);
    }
    if let Some(sha1) = &request.expected_sha1 {
        builder = builder.expected_sha1(sha1);
    }
    if let Some(sha256) = &request.expected_sha256 {
        builder = builder.expected_sha256(sha256);
    }
    builder.build()
}

fn verify_run_download_bytes(request: &RunDownloadRequest, bytes: &[u8]) -> Result<(), RunError> {
    if let Some(expected) = request.expected_size {
        let actual = bytes.len() as u64;
        if actual != expected {
            return Err(RunError::SizeMismatch {
                path: request.target.display().to_string(),
                expected,
                actual,
            });
        }
    }
    if let Some(expected) = &request.expected_sha1 {
        let actual = format!("{:x}", Sha1::digest(bytes));
        if !actual.eq_ignore_ascii_case(expected) {
            return Err(RunError::Sha1Mismatch {
                path: request.target.display().to_string(),
                expected: expected.clone(),
                actual,
            });
        }
    }
    if let Some(expected) = &request.expected_sha256 {
        let actual = format!("{:x}", sha2::Sha256::digest(bytes));
        if !actual.eq_ignore_ascii_case(expected) {
            return Err(RunError::ZuluChecksum {
                expected: expected.clone(),
                actual,
            });
        }
    }
    Ok(())
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
    java_version_major: u16,
    locator: &impl JavaLocator,
) -> Result<PathBuf, RunError> {
    if !config.java_path.trim().is_empty() {
        let path = PathBuf::from(config.java_path.trim());
        if path.is_file() {
            return Ok(path);
        }
        return Err(RunError::JavaNotFound {
            major: java_version_major,
        });
    }

    if let Some(path) = locator.path_java(java_version_major) {
        return Ok(path);
    }

    if let Some(path) = locator.common_java(java_version_major) {
        return Ok(path);
    }

    for path in project_java_candidates(project_dir, java_version_major) {
        if path.is_file() {
            return Ok(path);
        }
    }

    if let Some(path) = find_project_java(project_dir, java_version_major) {
        return Ok(path);
    }

    Err(RunError::JavaNotFound {
        major: java_version_major,
    })
}

/// Resolves Java by the configured lookup order, then installs a verified matching Zulu JDK.
///
/// The download path is intentionally reached only after every user-controlled and system
/// location has failed. Metadata and archive checksums are required; an unverifiable archive is
/// never extracted into `.mcmpu/java`.
pub fn resolve_or_download_java_executable(
    project_dir: &Path,
    config: &crate::config::TestRunConfig,
    java_version_major: u16,
    locator: &impl JavaLocator,
    downloader: &impl DownloadBytes,
) -> Result<PathBuf, RunError> {
    match resolve_java_executable(project_dir, config, java_version_major, locator) {
        Ok(path) => return Ok(path),
        Err(RunError::JavaNotFound { .. }) => {}
        Err(error) => return Err(error),
    }
    let major = java_version_major;
    let _lock = CacheFileLock::acquire(
        &zulu_platform_cache_root(project_dir, major)?.join("prepare.lock"),
    )?;
    let runtime = project_dir.join(format!(".mcmpu/java/{major}/runtime"));
    recover_directory_backup(&runtime)?;
    cleanup_orphaned_parts(&runtime)?;
    if let Some(java) = find_project_java(project_dir, major) {
        return Ok(java);
    }
    let metadata = fetch_zulu_metadata(project_dir, major, downloader)?;
    install_zulu_jdk(project_dir, major, &metadata, downloader)
}

const ZULU_METADATA_URL: &str = "https://api.azul.com/metadata/v1/zulu/packages";

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ZuluPackage {
    download_url: String,
    sha256_hash: String,
    name: String,
}

fn fetch_zulu_metadata(
    project_dir: &Path,
    major: u16,
    downloader: &impl DownloadBytes,
) -> Result<ZuluPackage, RunError> {
    let (os, arch, archive_type) = zulu_platform()?;
    let cache_path = zulu_platform_cache_root(project_dir, major)?.join("metadata.json");
    if cache_path.is_file() {
        match fs::read(&cache_path)
            .map_err(|source| RunError::ReadFile {
                path: cache_path.display().to_string(),
                source,
            })
            .and_then(|bytes| parse_zulu_metadata(major, os, arch, &bytes))
        {
            Ok(package) => return Ok(package),
            Err(_) => remove_file_if_exists(&cache_path)?,
        }
    }
    let url = format!(
        "{ZULU_METADATA_URL}/?java_version={major}&os={os}&arch={arch}&java_package_type=jdk&latest=true&release_status=ga&availability_types=CA&archive_type={archive_type}"
    );
    let bytes = downloader.download_bytes(&url)?;
    let package = parse_zulu_metadata(major, os, arch, &bytes)?;
    write_bytes_atomically(&cache_path, &bytes)?;
    Ok(package)
}

fn zulu_platform() -> Result<(&'static str, &'static str, &'static str), RunError> {
    let os = match std::env::consts::OS {
        "windows" => "windows",
        "linux" => "linux",
        "macos" => "macos",
        platform => {
            return Err(RunError::UnsupportedRuntimePlatform {
                platform: platform.to_string(),
            });
        }
    };
    let arch = match std::env::consts::ARCH {
        "x86_64" => "x64",
        "aarch64" => "aarch64",
        architecture => {
            return Err(RunError::ZuluMetadata {
                reason: format!(
                    "Azul does not provide an automatic JDK mapping for {architecture}"
                ),
            });
        }
    };
    Ok((os, arch, if os == "windows" { "zip" } else { "tar_gz" }))
}

fn parse_zulu_metadata(
    major: u16,
    os: &str,
    arch: &str,
    bytes: &[u8],
) -> Result<ZuluPackage, RunError> {
    let packages: Vec<ZuluPackage> =
        serde_json::from_slice(bytes).map_err(|source| RunError::ZuluMetadata {
            reason: format!("Azul metadata response is invalid: {source}"),
        })?;
    let package = packages
        .into_iter()
        .next()
        .ok_or_else(|| RunError::ZuluMetadata {
            reason: format!("Azul returned no GA JDK package for Java {major} on {os}-{arch}"),
        })?;
    if package.download_url.trim().is_empty()
        || package.name.trim().is_empty()
        || package.sha256_hash.len() != 64
        || !package
            .sha256_hash
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit())
    {
        return Err(RunError::ZuluMetadata {
            reason: "Azul metadata has an empty URL/name or invalid SHA-256".to_string(),
        });
    }
    Ok(package)
}

fn install_zulu_jdk(
    project_dir: &Path,
    major: u16,
    package: &ZuluPackage,
    downloader: &impl DownloadBytes,
) -> Result<PathBuf, RunError> {
    install_zulu_jdk_with_validation(project_dir, major, package, downloader, &|_| Ok(()))
}

fn install_zulu_jdk_with_validation(
    project_dir: &Path,
    major: u16,
    package: &ZuluPackage,
    downloader: &impl DownloadBytes,
    validate_candidate: &impl Fn(&Path) -> Result<(), RunError>,
) -> Result<PathBuf, RunError> {
    let cache_root = zulu_platform_cache_root(project_dir, major)?;
    create_dir(&cache_root)?;
    let extension = if package.name.ends_with(".tar.gz") {
        "tar.gz"
    } else if package.name.ends_with(".tgz") {
        "tgz"
    } else if package.name.ends_with(".zip") {
        "zip"
    } else {
        return Err(RunError::ZuluMetadata {
            reason: format!("unsupported Zulu archive extension in {}", package.name),
        });
    };
    let archive = cache_root.join(format!(
        "{}.{}",
        package.sha256_hash.to_ascii_lowercase(),
        extension
    ));
    recover_file_backup(&archive)?;
    cleanup_orphaned_parts(&archive)?;
    ensure_zulu_archive(&archive, package, downloader)?;
    let java_root = project_dir.join(format!(".mcmpu/java/{major}"));
    create_dir(&java_root)?;
    let runtime = java_root.join("runtime");
    recover_directory_backup(&runtime)?;
    cleanup_orphaned_parts(&runtime)?;
    let temporary = unique_sibling(&runtime, "part");
    create_dir(&temporary)?;
    let extraction_result = (|| {
        if package.name.ends_with(".zip") {
            extract_zulu_zip(&archive, &temporary)?;
        } else if package.name.ends_with(".tar.gz") || package.name.ends_with(".tgz") {
            extract_zulu_tar_gz(&archive, &temporary)?;
        }
        let candidate = find_java_under(&temporary).ok_or_else(|| RunError::ZuluMetadata {
            reason: format!(
                "Zulu archive '{}' did not contain {}",
                package.name,
                java_executable_name()
            ),
        })?;
        let relative =
            candidate
                .strip_prefix(&temporary)
                .map_err(|source| RunError::ZuluArchive {
                    reason: format!("failed to resolve extracted Java path: {source}"),
                })?;
        let relative = relative.to_path_buf();
        validate_candidate(&candidate)?;
        publish_directory_transactionally(&temporary, &runtime)?;
        Ok(runtime.join(relative))
    })();
    if temporary.exists() {
        remove_dir_if_exists(&temporary)?;
    }
    extraction_result
}

fn ensure_zulu_archive(
    archive: &Path,
    package: &ZuluPackage,
    downloader: &impl DownloadBytes,
) -> Result<(), RunError> {
    if archive.is_file() {
        if sha256_file(archive)?.eq_ignore_ascii_case(&package.sha256_hash) {
            return Ok(());
        }
        remove_file_if_exists(archive)?;
    }
    let requests = [RunDownloadRequest {
        task_id: format!("zulu-jdk-{}", package.sha256_hash),
        url: package.download_url.clone(),
        fallback_urls: Vec::new(),
        target: archive.to_path_buf(),
        expected_size: None,
        expected_sha1: None,
        expected_sha256: Some(package.sha256_hash.clone()),
    }];
    let mut results = downloader.download_many(&requests);
    if results.len() != 1 {
        return Err(RunError::InvalidDownloadBatch {
            reason: format!(
                "downloader returned {} result(s) for one Zulu archive",
                results.len()
            ),
        });
    }
    results.remove(0)?;
    let actual = sha256_file(archive)?;
    if !actual.eq_ignore_ascii_case(&package.sha256_hash) {
        remove_file_if_exists(archive)?;
        return Err(RunError::ZuluChecksum {
            expected: package.sha256_hash.clone(),
            actual,
        });
    }
    Ok(())
}

fn sha256_file(path: &Path) -> Result<String, RunError> {
    let mut file = fs::File::open(path).map_err(|source| RunError::ReadFile {
        path: path.display().to_string(),
        source,
    })?;
    let mut digest = sha2::Sha256::new();
    let mut buffer = [0_u8; 256 * 1024];
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|source| RunError::ReadFile {
                path: path.display().to_string(),
                source,
            })?;
        if read == 0 {
            break;
        }
        digest.update(&buffer[..read]);
    }
    Ok(format!("{:x}", digest.finalize()))
}

fn zulu_major_cache_root(project_dir: &Path, major: u16) -> PathBuf {
    project_dir.join(format!(".mcmpu/cache/java/{major}"))
}

fn zulu_platform_cache_root(project_dir: &Path, major: u16) -> Result<PathBuf, RunError> {
    let (os, arch, _) = zulu_platform()?;
    Ok(zulu_major_cache_root(project_dir, major).join(format!("{os}-{arch}")))
}

fn extract_zulu_zip(path: &Path, destination: &Path) -> Result<(), RunError> {
    let file = fs::File::open(path).map_err(|source| RunError::ReadFile {
        path: path.display().to_string(),
        source,
    })?;
    let mut archive = zip::ZipArchive::new(file).map_err(|source| RunError::ZuluArchive {
        reason: source.to_string(),
    })?;
    for index in 0..archive.len() {
        let mut entry = archive
            .by_index(index)
            .map_err(|source| RunError::ZuluArchive {
                reason: source.to_string(),
            })?;
        let path = entry.enclosed_name().ok_or_else(|| RunError::ZuluArchive {
            reason: format!("unsafe archive entry {}", entry.name()),
        })?;
        let target = destination.join(path);
        if entry.is_dir() {
            create_dir(&target)?;
            continue;
        }
        let parent = target.parent().ok_or_else(|| RunError::ZuluArchive {
            reason: format!("archive entry {} has no parent", entry.name()),
        })?;
        create_dir(parent)?;
        let mut output = fs::File::create(&target).map_err(|source| RunError::WriteFile {
            path: target.display().to_string(),
            source,
        })?;
        std::io::copy(&mut entry, &mut output).map_err(|source| RunError::ZuluArchive {
            reason: source.to_string(),
        })?;
    }
    Ok(())
}

fn extract_zulu_tar_gz(path: &Path, destination: &Path) -> Result<(), RunError> {
    let file = fs::File::open(path).map_err(|source| RunError::ReadFile {
        path: path.display().to_string(),
        source,
    })?;
    let decoder = flate2::read::GzDecoder::new(file);
    let mut archive = tar::Archive::new(decoder);
    for entry in archive.entries().map_err(|source| RunError::ZuluArchive {
        reason: source.to_string(),
    })? {
        let mut entry = entry.map_err(|source| RunError::ZuluArchive {
            reason: source.to_string(),
        })?;
        if !entry
            .unpack_in(destination)
            .map_err(|source| RunError::ZuluArchive {
                reason: source.to_string(),
            })?
        {
            return Err(RunError::ZuluArchive {
                reason: "archive entry escaped the Java installation directory".to_string(),
            });
        }
    }
    Ok(())
}

/// Rejects the legacy launch API because it cannot produce a real loader command
/// without a [`LaunchDescriptor`].
///
/// Call [`plan_launch_command_from_descriptor`] for executable launch planning.
pub fn plan_launch_command(
    _plan: &RunLaunchPlan,
    _java_executable: &Path,
) -> Result<LaunchCommand, RunError> {
    Err(RunError::LaunchDescriptorRequired)
}

/// Builds the complete shell-free Java command for a prepared run and resolved descriptor.
///
/// Memory and debug arguments from `plan` are placed first, followed by the descriptor's
/// substituted JVM arguments, a classpath when the descriptor did not already provide one,
/// the resolved main class, and substituted game arguments. Client substitutions use a
/// deterministic offline identity. Server commands append `nogui` only when the descriptor
/// does not already contain either `nogui` or `--nogui`. The prepared run directory and Java
/// executable must already exist; both are resolved to stable absolute paths before the child
/// working directory is applied.
pub fn plan_launch_command_from_descriptor(
    plan: &RunLaunchPlan,
    descriptor: &LaunchDescriptor,
    java_executable: &Path,
) -> Result<LaunchCommand, RunError> {
    let expected_side = match plan.side {
        RunSide::Client => LaunchSide::Client,
        RunSide::Server => LaunchSide::Server,
    };
    if descriptor.side != expected_side {
        return Err(RunError::LaunchSideMismatch {
            plan: plan.side,
            descriptor: descriptor.side,
        });
    }
    if descriptor.main_class.trim().is_empty() {
        return Err(RunError::EmptyMainClass);
    }

    let run_dir = canonical_launch_run_directory(&plan.run_dir)?;
    let java_executable = canonical_launch_java_executable(java_executable)?;
    let classpath_separator = if cfg!(windows) { ";" } else { ":" };
    let classpath = descriptor
        .classpath
        .iter()
        .map(|entry| {
            let path = Path::new(entry);
            if path.is_absolute() {
                path.to_path_buf()
            } else {
                run_dir.join(path)
            }
        })
        .map(|path| path.to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join(classpath_separator);
    let assets_root = run_dir.join("assets");
    let natives_directory = run_dir.join("natives");
    let libraries_directory = run_dir.join("libraries");
    let minecraft_version_id =
        descriptor
            .inheritance_chain
            .first()
            .ok_or_else(|| RunError::InvalidLaunchDescriptor {
                reason: format!(
                    "launch descriptor '{}' has an empty inheritance chain",
                    descriptor.version_id
                ),
            })?;
    let mut variables = BTreeMap::from([
        ("version_name".to_string(), minecraft_version_id.clone()),
        (
            "game_directory".to_string(),
            run_dir.to_string_lossy().into_owned(),
        ),
        (
            "assets_root".to_string(),
            assets_root.to_string_lossy().into_owned(),
        ),
        (
            "game_assets".to_string(),
            assets_root.to_string_lossy().into_owned(),
        ),
        (
            "natives_directory".to_string(),
            natives_directory.to_string_lossy().into_owned(),
        ),
        (
            "library_directory".to_string(),
            libraries_directory.to_string_lossy().into_owned(),
        ),
        ("classpath".to_string(), classpath.clone()),
        (
            "classpath_separator".to_string(),
            classpath_separator.to_string(),
        ),
        ("launcher_name".to_string(), "mcmpu".to_string()),
        (
            "launcher_version".to_string(),
            crate::BUILD_VERSION.to_string(),
        ),
        ("version_type".to_string(), "release".to_string()),
    ]);
    if let Some(asset_index) = &descriptor.asset_index {
        variables.insert("assets_index_name".to_string(), asset_index.id.clone());
    } else if let Some(assets) = &descriptor.assets {
        variables.insert("assets_index_name".to_string(), assets.clone());
    }
    if plan.side == RunSide::Client {
        variables.extend([
            ("auth_player_name".to_string(), plan.player_name.clone()),
            ("auth_uuid".to_string(), plan.offline_uuid.clone()),
            (
                "auth_access_token".to_string(),
                OFFLINE_ACCESS_TOKEN.to_string(),
            ),
            ("user_type".to_string(), "legacy".to_string()),
            ("user_properties".to_string(), "{}".to_string()),
            ("clientid".to_string(), String::new()),
            ("auth_xuid".to_string(), String::new()),
        ]);
    }

    let (mut jvm_arguments, mut game_arguments) = descriptor.substitute_arguments(&variables)?;
    if plan.side == RunSide::Client
        && !jvm_arguments
            .iter()
            .any(|argument| argument.starts_with("-Djava.library.path="))
    {
        jvm_arguments.insert(
            0,
            format!("-Djava.library.path={}", natives_directory.display()),
        );
    }
    let descriptor_has_classpath = jvm_arguments
        .iter()
        .any(|argument| matches!(argument.as_str(), "-cp" | "-classpath" | "--class-path"));
    if plan.side == RunSide::Server
        && !game_arguments
            .iter()
            .any(|argument| matches!(argument.to_ascii_lowercase().as_str(), "nogui" | "--nogui"))
    {
        game_arguments.push("nogui".to_string());
    }

    let mut args = plan.java_args.clone();
    args.extend(jvm_arguments);
    if !descriptor_has_classpath {
        args.push("-cp".to_string());
        args.push(classpath);
    }
    args.push(descriptor.main_class.clone());
    args.extend(game_arguments);
    Ok(LaunchCommand {
        side: plan.side,
        working_dir: run_dir,
        executable: java_executable,
        args,
    })
}

fn canonical_launch_run_directory(path: &Path) -> Result<PathBuf, RunError> {
    let canonical = fs::canonicalize(path).map_err(|source| RunError::LaunchRunDirectory {
        path: path.to_path_buf(),
        source,
    })?;
    if !canonical.is_dir() {
        return Err(RunError::LaunchRunDirectory {
            path: path.to_path_buf(),
            source: std::io::Error::new(
                std::io::ErrorKind::NotADirectory,
                "resolved path is not a directory",
            ),
        });
    }
    Ok(java_compatible_canonical_path(canonical))
}

fn canonical_launch_java_executable(path: &Path) -> Result<PathBuf, RunError> {
    let candidate = if path.is_absolute() {
        path.to_path_buf()
    } else if path.components().count() == 1 {
        let name = path
            .to_str()
            .ok_or_else(|| RunError::LaunchJavaExecutable {
                path: path.to_path_buf(),
                source: std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "executable name is not valid UTF-8",
                ),
            })?;
        find_on_path(name)
            .or_else(|| {
                if cfg!(windows) && path.extension().is_none() {
                    find_on_path(&format!("{name}.exe"))
                } else {
                    None
                }
            })
            .ok_or_else(|| RunError::LaunchJavaExecutable {
                path: path.to_path_buf(),
                source: std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    "executable was not found on PATH",
                ),
            })?
    } else {
        env::current_dir()
            .map_err(|source| RunError::LaunchJavaExecutable {
                path: path.to_path_buf(),
                source,
            })?
            .join(path)
    };
    let canonical =
        fs::canonicalize(&candidate).map_err(|source| RunError::LaunchJavaExecutable {
            path: path.to_path_buf(),
            source,
        })?;
    if !canonical.is_file() {
        return Err(RunError::LaunchJavaExecutable {
            path: path.to_path_buf(),
            source: std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "resolved path is not a file",
            ),
        });
    }
    Ok(java_compatible_canonical_path(canonical))
}

#[cfg(not(windows))]
fn java_compatible_canonical_path(path: PathBuf) -> PathBuf {
    path
}

#[cfg(windows)]
fn java_compatible_canonical_path(path: PathBuf) -> PathBuf {
    use std::os::windows::ffi::{OsStrExt, OsStringExt};

    const VERBATIM_PREFIX: &[u16] = &[b'\\' as u16, b'\\' as u16, b'?' as u16, b'\\' as u16];
    const VERBATIM_UNC_PREFIX: &[u16] = &[
        b'\\' as u16,
        b'\\' as u16,
        b'?' as u16,
        b'\\' as u16,
        b'U' as u16,
        b'N' as u16,
        b'C' as u16,
        b'\\' as u16,
    ];
    let encoded = path.as_os_str().encode_wide().collect::<Vec<_>>();
    if let Some(remainder) = encoded.strip_prefix(VERBATIM_UNC_PREFIX) {
        let mut conventional = vec![b'\\' as u16, b'\\' as u16];
        conventional.extend_from_slice(remainder);
        return PathBuf::from(OsString::from_wide(&conventional));
    }
    if let Some(remainder) = encoded.strip_prefix(VERBATIM_PREFIX) {
        return PathBuf::from(OsString::from_wide(remainder));
    }
    path
}

/// Spawns and supervises one launch command, then always scans pending sync changes.
///
/// The child is polled with [`RunningProcess::try_wait`]. A server receives Minecraft's
/// `stop` command when `stop_signal` is raised, while a client receives the platform process
/// termination request. If the process has not exited by `timeout`, it is force-terminated
/// and reaped. `event_callback` receives ordered lifecycle and pending-sync transitions.
/// Non-zero process statuses are returned in [`RunOutcome`] rather than converted into errors.
pub fn supervise_run<R, S, F>(
    project_dir: &Path,
    command: &LaunchCommand,
    runner: &R,
    stop_signal: &S,
    console: &dyn ConsoleInput,
    timeout: Duration,
    event_callback: &mut F,
) -> Result<RunOutcome, RunError>
where
    R: ProcessRunner + ?Sized,
    S: StopSignal + ?Sized,
    F: FnMut(&RunEvent),
{
    event_callback(&RunEvent::Spawning {
        command: ProcessSpec::from(command).display(),
    });
    let lifecycle = supervise_process(
        command,
        runner,
        stop_signal,
        console,
        timeout,
        event_callback,
    );
    let pending = scan_pending_sync(project_dir, &command.working_dir, command.side);
    if let Ok(pending_sync) = &pending {
        event_callback(&RunEvent::PendingSyncWritten {
            path: pending_sync.path.clone(),
            entries: pending_sync.entries.len(),
        });
    }

    match (lifecycle, pending) {
        (Ok((pid, status, termination)), Ok(pending_sync)) => Ok(RunOutcome {
            pid,
            status,
            termination,
            pending_sync,
        }),
        (Err(error), Ok(_)) => Err(error),
        (Ok(_), Err(error)) => Err(error),
        (Err(lifecycle), Err(pending_sync)) => Err(RunError::LifecycleAndPendingSync {
            lifecycle: Box::new(lifecycle),
            pending_sync: Box::new(pending_sync),
        }),
    }
}

fn supervise_process<R, S, F>(
    command: &LaunchCommand,
    runner: &R,
    stop_signal: &S,
    console: &dyn ConsoleInput,
    timeout: Duration,
    event_callback: &mut F,
) -> Result<(u32, ExitStatus, RunTermination), RunError>
where
    R: ProcessRunner + ?Sized,
    S: StopSignal + ?Sized,
    F: FnMut(&RunEvent),
{
    let mut process = runner.spawn(&ProcessSpec::from(command))?;
    let pid = process.pid();
    event_callback(&RunEvent::Spawned { pid });
    let mut stop_started = None;
    let mut termination = RunTermination::Exited;

    loop {
        process.drain_output(&mut |stream, line| {
            event_callback(&RunEvent::ProcessOutput { pid, stream, line });
        })?;
        // Typed console lines go to the child's stdin. A failed write means the process already
        // exited, which the exit check below reports; it must not fail the run by itself.
        for line in console.take_lines() {
            let _ = process.write_stdin(
                format!(
                    "{line}
"
                )
                .as_bytes(),
            );
        }
        if let Some(status) = process.try_wait()? {
            process.finish_output(&mut |stream, line| {
                event_callback(&RunEvent::ProcessOutput { pid, stream, line });
            })?;
            event_callback(&RunEvent::Exited {
                pid,
                success: status.success(),
            });
            return Ok((pid, status, termination));
        }

        if stop_started.is_none() && stop_signal.stop_requested() {
            event_callback(&RunEvent::StopRequested { pid });
            termination = RunTermination::StopRequested;
            match command.side {
                RunSide::Server => process.graceful_stop()?,
                RunSide::Client => process.terminate()?,
            }
            stop_started = Some(Instant::now());
        }

        if stop_started.is_some_and(|started| started.elapsed() >= timeout) {
            process.terminate()?;
            termination = RunTermination::Forced;
            event_callback(&RunEvent::ForcedTermination { pid });
            let status = process.wait()?;
            process.finish_output(&mut |stream, line| {
                event_callback(&RunEvent::ProcessOutput { pid, stream, line });
            })?;
            event_callback(&RunEvent::Exited {
                pid,
                success: status.success(),
            });
            return Ok((pid, status, termination));
        }

        thread::sleep(RUN_SUPERVISOR_POLL_INTERVAL);
    }
}

pub fn run_launch_command(
    project_dir: &Path,
    command: &LaunchCommand,
    runner: &impl ProcessRunner,
) -> Result<PathBuf, RunError> {
    run_launch_command_with_stop_signal(project_dir, command, runner, &NeverStop)
}

/// Launches Minecraft while honoring external cancellation.
///
/// Pending-sync scanning is delegated to [`supervise_run`], so it runs after normal exits,
/// explicit cancellation, and forced termination alike.
pub fn run_launch_command_with_stop_signal<S>(
    project_dir: &Path,
    command: &LaunchCommand,
    runner: &impl ProcessRunner,
    stop_signal: &S,
) -> Result<PathBuf, RunError>
where
    S: StopSignal + ?Sized,
{
    run_launch_command_with_stop_signal_and_events(
        project_dir,
        command,
        runner,
        stop_signal,
        &mut |_| {},
    )
}

fn run_launch_command_with_stop_signal_and_events<S, F>(
    project_dir: &Path,
    command: &LaunchCommand,
    runner: &impl ProcessRunner,
    stop_signal: &S,
    event_callback: &mut F,
) -> Result<PathBuf, RunError>
where
    S: StopSignal + ?Sized,
    F: FnMut(&RunEvent),
{
    let outcome = supervise_run(
        project_dir,
        command,
        runner,
        stop_signal,
        &NeverConsole,
        GRACEFUL_SHUTDOWN_TIMEOUT,
        event_callback,
    )?;
    if outcome.termination != RunTermination::Exited {
        return Err(RunError::Cancelled);
    }
    if !outcome.status.success() {
        return Err(RunError::ProcessExit {
            status: outcome.status,
        });
    }
    Ok(outcome.pending_sync.path)
}

fn run_process_with_stop_signal_and_events<S, F>(
    command: &LaunchCommand,
    runner: &impl ProcessRunner,
    stop_signal: &S,
    console: &dyn ConsoleInput,
    event_callback: &mut F,
) -> Result<(), RunError>
where
    S: StopSignal + ?Sized,
    F: FnMut(&RunEvent),
{
    event_callback(&RunEvent::Spawning {
        command: ProcessSpec::from(command).display(),
    });
    let (_, status, termination) = supervise_process(
        command,
        runner,
        stop_signal,
        console,
        GRACEFUL_SHUTDOWN_TIMEOUT,
        event_callback,
    )?;
    if termination != RunTermination::Exited {
        return Err(RunError::Cancelled);
    }
    if !status.success() {
        return Err(RunError::ProcessExit { status });
    }
    Ok(())
}

pub fn offline_access_token() -> &'static str {
    OFFLINE_ACCESS_TOKEN
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
        .ok_or(RunError::MissingCurseForgeRuntimeCoordinate)?;
    let file_id = resolved
        .entry
        .file_id
        .ok_or(RunError::MissingCurseForgeRuntimeCoordinate)?;
    let slug = resolved
        .entry
        .slug
        .as_deref()
        .ok_or(RunError::MissingCurseForgeRuntimeCoordinate)?;
    let file_name = run_file_name(&resolved.entry.dependency_type, slug, &resolved.file);
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
        url: curseforge_cdn_artifact_url(&resolved.file)?,
        cache_path: project_dir
            .join(".mcmpu")
            .join("cache")
            .join(folder)
            .join(file_name),
        sha1,
        size: resolved.file.file_length,
    })
}

fn java_args_for_side(config: &Config, side: RunSide) -> Result<Vec<String>, RunError> {
    let java = match side {
        RunSide::Client => &config.java.client,
        RunSide::Server => &config.java.server,
    };
    let mut arguments = java_memory_arguments(java)?;
    append_extra_arguments(&mut arguments, &config.test_run.debug_extra_jvm_args)?;
    Ok(arguments)
}

fn current_launch_environment() -> Result<crate::project::launcher::LaunchEnvironment, RunError> {
    let os_name = match std::env::consts::OS {
        "windows" => "windows",
        "linux" => "linux",
        "macos" => "osx",
        other => {
            return Err(RunError::UnsupportedRuntimePlatform {
                platform: other.to_string(),
            });
        }
    };
    let arch_bits = if cfg!(target_pointer_width = "64") {
        64
    } else {
        32
    };
    Ok(crate::project::launcher::LaunchEnvironment::new(
        os_name,
        std::env::consts::ARCH,
        arch_bits,
    ))
}

/// Makes sure the directories mcmpu fills exist, without emptying them.
///
/// Mods keep files of their own in here — Forgelin unpacks its Scala libraries into
/// `mods/<mcversion>/` — and wiping those makes the mod write them again on every start, with
/// fresh timestamps that invalidate every cache keyed on them. Files mcmpu wrote itself and no
/// longer wants are removed by [`prune_unmanaged_generated_files`] instead.
fn ensure_generated_dirs(run_dir: &Path) -> Result<(), RunError> {
    for dir in GENERATED_DIRS {
        create_dir(run_dir.join(dir))?;
    }
    Ok(())
}

/// Removes files mcmpu wrote into a generated directory on an earlier run that this run does not
/// want any more.
///
/// Only the top level is cleaned: everything below it belongs to mods, which write there while
/// the game runs.
fn prune_unmanaged_generated_files(
    run_dir: &Path,
    managed: &BTreeSet<String>,
) -> Result<(), RunError> {
    for dir in GENERATED_DIRS {
        let root = run_dir.join(dir);
        let entries = match fs::read_dir(&root) {
            Ok(entries) => entries,
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => continue,
            Err(source) => {
                return Err(RunError::ReadDir {
                    path: root.display().to_string(),
                    source,
                });
            }
        };
        for entry in entries {
            let entry = entry.map_err(|source| RunError::ReadDir {
                path: root.display().to_string(),
                source,
            })?;
            let file_type = entry.file_type().map_err(|source| RunError::ReadDir {
                path: entry.path().display().to_string(),
                source,
            })?;
            if !file_type.is_file() {
                continue;
            }
            let Ok(name) = entry.file_name().into_string() else {
                continue;
            };
            if managed.contains(&format!("{dir}/{name}")) {
                continue;
            }
            fs::remove_file(entry.path()).map_err(|source| RunError::RemoveFile {
                path: entry.path().display().to_string(),
                source,
            })?;
        }
    }
    Ok(())
}

fn copy_override_layers(
    project_dir: &Path,
    run_dir: &Path,
    side: RunSide,
    sources: &mut BTreeMap<String, RunSourceEntry>,
) -> Result<(), RunError> {
    copy_tree_if_exists(
        &project_dir.join("overrides").join("common"),
        run_dir,
        RunSourceKind::OverridesCommon,
        sources,
    )?;
    copy_tree_if_exists(
        &project_dir.join("overrides").join(side.dir_name()),
        run_dir,
        match side {
            RunSide::Client => RunSourceKind::OverridesClient,
            RunSide::Server => RunSourceKind::OverridesServer,
        },
        sources,
    )?;
    Ok(())
}

fn copy_debug_layers(
    project_dir: &Path,
    run_dir: &Path,
    side: RunSide,
    sources: &mut BTreeMap<String, RunSourceEntry>,
) -> Result<(), RunError> {
    copy_tree_if_exists(
        &project_dir.join("debug").join("common"),
        run_dir,
        RunSourceKind::DebugCommon,
        sources,
    )?;
    copy_tree_if_exists(
        &project_dir.join("debug").join(side.dir_name()),
        run_dir,
        match side {
            RunSide::Client => RunSourceKind::DebugClient,
            RunSide::Server => RunSourceKind::DebugServer,
        },
        sources,
    )?;
    Ok(())
}

/// Re-applies the project overlay layers over an already prepared run directory.
///
/// The layers are copied once while the run directory is prepared, but the module download
/// and the loader/artifact installation write into the same directories afterwards
/// (`mods`, `resourcepacks`, `shaderpacks`, `libraries`, `versions`, `assets`, `natives`).
/// Applying the layers again last keeps project overrides authoritative for any path that
/// both steps touch, such as a pinned native or a locally patched mod archive.
pub fn apply_overlay_layers(
    project_dir: &Path,
    run_dir: &Path,
    side: RunSide,
) -> Result<(), RunError> {
    let mut sources = BTreeMap::new();
    copy_override_layers(project_dir, run_dir, side, &mut sources)?;
    copy_debug_layers(project_dir, run_dir, side, &mut sources)?;
    write_source_manifest(run_dir, side, sources)
}

/// Reports whether the project configures any overlay layer for one run side.
fn has_overlay_layers(project_dir: &Path, side: RunSide) -> bool {
    ["common", side.dir_name()].iter().any(|name| {
        project_dir.join("overrides").join(name).exists()
            || project_dir.join("debug").join(name).exists()
    })
}

/// Applies [`apply_overlay_layers`] immediately before the game process starts.
fn reapply_overlay_layers(
    project_dir: &Path,
    run_dir: &Path,
    side: RunSide,
    progress: &mut impl FnMut(&RunProgress),
) -> Result<(), RunError> {
    if !has_overlay_layers(project_dir, side) {
        return Ok(());
    }
    progress(&RunProgress::CopyingOverrides { side });
    progress(&RunProgress::CopyingDebug { side });
    apply_overlay_layers(project_dir, run_dir, side)
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
        // Overlays are applied again on every start, so an identical copy is left untouched:
        // rewriting it would move a mtime that mods key their caches on, and re-copy the tree.
        if !target_matches_source(entry.path(), &target) {
            if let Some(parent) = target.parent() {
                create_dir(parent)?;
            }
            copy_file_keeping_mtime(entry.path(), &target)?;
        }
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
    let config = sync_config_for_project(project_dir, side)?;
    write_sync_config_for_project(project_dir, side, &config)
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
    let mut file = fs::File::open(path).map_err(|source| RunError::ReadFile {
        path: path.display().to_string(),
        source,
    })?;
    let size = file
        .metadata()
        .map_err(|source| RunError::ReadFile {
            path: path.display().to_string(),
            source,
        })?
        .len();
    let mut hasher = Sha1::new();
    let mut buffer = [0_u8; 256 * 1024];
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|source| RunError::ReadFile {
                path: path.display().to_string(),
                source,
            })?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(FileFingerprint {
        sha1: format!("{:x}", hasher.finalize()),
        size,
    })
}

fn is_valid_file(path: &Path, expected_size: u64, expected_sha1: &str) -> Result<bool, RunError> {
    match file_fingerprint(path) {
        Ok(fingerprint) => Ok(fingerprint.size == expected_size
            && fingerprint.sha1.eq_ignore_ascii_case(expected_sha1)),
        Err(RunError::ReadFile { source, .. }) if source.kind() == std::io::ErrorKind::NotFound => {
            Ok(false)
        }
        Err(error) => Err(error),
    }
}

struct CacheFileLock {
    path: PathBuf,
    file: fs::File,
}

impl CacheFileLock {
    fn acquire(path: &Path) -> Result<Self, RunError> {
        let parent = path.parent().ok_or_else(|| RunError::CacheLockTimeout {
            path: path.display().to_string(),
        })?;
        create_dir(parent)?;
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)
            .map_err(|source| RunError::WriteFile {
                path: path.display().to_string(),
                source,
            })?;
        let started = Instant::now();
        loop {
            match file.try_lock() {
                Ok(()) => {
                    file.set_len(0)
                        .and_then(|()| file.seek(SeekFrom::Start(0)))
                        .and_then(|_| writeln!(file, "pid={}", std::process::id()))
                        .and_then(|()| file.sync_all())
                        .map_err(|source| RunError::WriteFile {
                            path: path.display().to_string(),
                            source,
                        })?;
                    return Ok(Self {
                        path: path.to_path_buf(),
                        file,
                    });
                }
                Err(std::fs::TryLockError::WouldBlock) => {
                    if started.elapsed() >= CACHE_LOCK_WAIT_TIMEOUT {
                        return Err(RunError::CacheLockTimeout {
                            path: path.display().to_string(),
                        });
                    }
                    thread::sleep(Duration::from_millis(100));
                }
                Err(std::fs::TryLockError::Error(source)) => {
                    return Err(RunError::WriteFile {
                        path: path.display().to_string(),
                        source,
                    });
                }
            }
        }
    }
}

impl Drop for CacheFileLock {
    fn drop(&mut self) {
        if let Err(error) = self.file.unlock() {
            eprintln!(
                "failed to release cache lock '{}': {error}",
                self.path.display()
            );
        }
    }
}

fn unique_sibling(path: &Path, suffix: &str) -> PathBuf {
    let sequence = CACHE_TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "cache".to_string());
    path.with_file_name(format!(
        ".{name}.{suffix}-{}-{sequence}",
        std::process::id()
    ))
}

fn write_bytes_atomically(path: &Path, bytes: &[u8]) -> Result<(), RunError> {
    if let Some(parent) = path.parent() {
        create_dir(parent)?;
    }
    let temporary = unique_sibling(path, "part");
    let write_result = (|| {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
            .map_err(|source| RunError::WriteFile {
                path: temporary.display().to_string(),
                source,
            })?;
        file.write_all(bytes)
            .and_then(|()| file.sync_all())
            .map_err(|source| RunError::WriteFile {
                path: temporary.display().to_string(),
                source,
            })?;
        publish_file_transactionally(&temporary, path)
    })();
    if temporary.exists() {
        remove_file_if_exists(&temporary)?;
    }
    write_result
}

fn copy_file_atomically(source: &Path, target: &Path) -> Result<(), RunError> {
    let temporary = unique_sibling(target, "part");
    let copy_result = (|| {
        let mut input = fs::File::open(source).map_err(|error| RunError::ReadFile {
            path: source.display().to_string(),
            source: error,
        })?;
        let mut output = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
            .map_err(|error| RunError::WriteFile {
                path: temporary.display().to_string(),
                source: error,
            })?;
        std::io::copy(&mut input, &mut output)
            .and_then(|_| {
                // The copy takes over the source's timestamp: mods key their caches on jar mtimes,
                // so a file stamped "now" on every start invalidates all of them.
                if let Some(modified) = source_modified(source) {
                    let _ = output.set_modified(modified);
                }
                output.sync_all()
            })
            .map_err(|error| RunError::CopyFile {
                from: source.display().to_string(),
                to: temporary.display().to_string(),
                source: error,
            })?;
        publish_file_transactionally(&temporary, target)
    })();
    if temporary.exists() {
        remove_file_if_exists(&temporary)?;
    }
    copy_result
}

/// Copies `source` over `target`, giving the copy the source's modification time.
fn copy_file_keeping_mtime(source: &Path, target: &Path) -> Result<(), RunError> {
    fs::copy(source, target).map_err(|error| RunError::CopyFile {
        from: source.display().to_string(),
        to: target.display().to_string(),
        source: error,
    })?;
    if let Some(modified) = source_modified(source) {
        apply_file_mtime(target, modified);
    }
    Ok(())
}

fn source_modified(path: &Path) -> Option<SystemTime> {
    fs::metadata(path)
        .and_then(|metadata| metadata.modified())
        .ok()
}

/// Best effort: a file system that refuses the timestamp must not fail an otherwise good copy.
fn apply_file_mtime(path: &Path, modified: SystemTime) {
    if let Ok(file) = OpenOptions::new().write(true).open(path) {
        let _ = file.set_modified(modified);
    }
}

/// True when `target` already holds the same bytes as `source`, judged by size and timestamp.
///
/// Every copy mcmpu makes keeps the source's timestamp, so a match here means the file was
/// written from this source and does not need to be read back or written again.
fn target_matches_source(source: &Path, target: &Path) -> bool {
    let (Ok(source_meta), Ok(target_meta)) = (fs::metadata(source), fs::metadata(target)) else {
        return false;
    };
    if source_meta.len() != target_meta.len()
        || source_meta.modified().ok() != target_meta.modified().ok()
    {
        return false;
    }

    // Windows can give files created in quick succession the same timestamp. Equal
    // timestamps and lengths therefore do not prove that two overlay files have the
    // same contents (for example, `common` and `client` replacements of equal size).
    let (Ok(mut source), Ok(mut target)) = (fs::File::open(source), fs::File::open(target)) else {
        return false;
    };
    let mut source_buffer = [0_u8; 64 * 1024];
    let mut target_buffer = [0_u8; 64 * 1024];
    loop {
        let (Ok(source_len), Ok(target_len)) = (
            source.read(&mut source_buffer),
            target.read(&mut target_buffer),
        ) else {
            return false;
        };
        if source_len != target_len || source_buffer[..source_len] != target_buffer[..target_len] {
            return false;
        }
        if source_len == 0 {
            return true;
        }
    }
}

fn publish_file_transactionally(temporary: &Path, target: &Path) -> Result<(), RunError> {
    recover_file_backup(target)?;
    if !target.exists() {
        return fs::rename(temporary, target).map_err(|source| RunError::WriteFile {
            path: target.display().to_string(),
            source,
        });
    }
    let backup = unique_sibling(target, "backup");
    fs::rename(target, &backup).map_err(|source| RunError::WriteFile {
        path: target.display().to_string(),
        source,
    })?;
    if let Err(source) = fs::rename(temporary, target) {
        let restore = fs::rename(&backup, target);
        return Err(RunError::WriteFile {
            path: target.display().to_string(),
            source: restore.err().unwrap_or(source),
        });
    }
    remove_file_if_exists(&backup)
}

fn publish_directory_transactionally(temporary: &Path, target: &Path) -> Result<(), RunError> {
    recover_directory_backup(target)?;
    if !target.exists() {
        return fs::rename(temporary, target).map_err(|source| RunError::WriteFile {
            path: target.display().to_string(),
            source,
        });
    }
    let backup = unique_sibling(target, "backup");
    fs::rename(target, &backup).map_err(|source| RunError::WriteFile {
        path: target.display().to_string(),
        source,
    })?;
    if let Err(source) = fs::rename(temporary, target) {
        let restore = fs::rename(&backup, target);
        return Err(RunError::WriteFile {
            path: target.display().to_string(),
            source: restore.err().unwrap_or(source),
        });
    }
    remove_dir_if_exists(&backup)
}

fn recover_file_backup(target: &Path) -> Result<(), RunError> {
    recover_transactional_backup(target, false)
}

fn recover_directory_backup(target: &Path) -> Result<(), RunError> {
    recover_transactional_backup(target, true)
}

fn recover_transactional_backup(target: &Path, directory: bool) -> Result<(), RunError> {
    let Some(parent) = target.parent() else {
        return Ok(());
    };
    if !parent.exists() {
        return Ok(());
    }
    let name = target
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "cache".to_string());
    let prefix = format!(".{name}.backup-");
    let mut backups = fs::read_dir(parent)
        .map_err(|source| RunError::ReadFile {
            path: parent.display().to_string(),
            source,
        })?
        .map(|entry| {
            entry.map_err(|source| RunError::ReadFile {
                path: parent.display().to_string(),
                source,
            })
        })
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .filter(|entry| {
            entry.file_name().to_string_lossy().starts_with(&prefix)
                && entry.file_type().is_ok_and(|kind| {
                    if directory {
                        kind.is_dir()
                    } else {
                        kind.is_file()
                    }
                })
        })
        .map(|entry| entry.path())
        .collect::<Vec<_>>();
    if backups.is_empty() {
        return Ok(());
    }
    backups.sort_by(|left, right| {
        let left_time = fs::metadata(left)
            .and_then(|metadata| metadata.modified())
            .ok();
        let right_time = fs::metadata(right)
            .and_then(|metadata| metadata.modified())
            .ok();
        left_time.cmp(&right_time).then_with(|| left.cmp(right))
    });
    if !target.exists() {
        let restore = backups.pop().expect("nonempty backups were checked above");
        fs::rename(&restore, target).map_err(|source| RunError::WriteFile {
            path: target.display().to_string(),
            source,
        })?;
    }
    for backup in backups {
        if directory {
            remove_dir_if_exists(&backup)?;
        } else {
            remove_file_if_exists(&backup)?;
        }
    }
    Ok(())
}

fn cleanup_orphaned_parts(target: &Path) -> Result<(), RunError> {
    let Some(parent) = target.parent() else {
        return Ok(());
    };
    if !parent.exists() {
        return Ok(());
    }
    let name = target
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "cache".to_string());
    let prefix = format!(".{name}.part-");
    for entry in fs::read_dir(parent).map_err(|source| RunError::ReadFile {
        path: parent.display().to_string(),
        source,
    })? {
        let entry = entry.map_err(|source| RunError::ReadFile {
            path: parent.display().to_string(),
            source,
        })?;
        if !entry.file_name().to_string_lossy().starts_with(&prefix) {
            continue;
        }
        let path = entry.path();
        if entry
            .file_type()
            .map_err(|source| RunError::ReadFile {
                path: path.display().to_string(),
                source,
            })?
            .is_dir()
        {
            remove_dir_if_exists(&path)?;
        } else {
            remove_file_if_exists(&path)?;
        }
    }
    Ok(())
}

fn remove_file_if_exists(path: &Path) -> Result<(), RunError> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(RunError::RemoveFile {
            path: path.display().to_string(),
            source,
        }),
    }
}

fn remove_dir_if_exists(path: &Path) -> Result<(), RunError> {
    match fs::remove_dir_all(path) {
        Ok(()) => Ok(()),
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(RunError::RemoveDir {
            path: path.display().to_string(),
            source,
        }),
    }
}

/// Collects the files a run directory gained under one sync include path.
///
/// A newly added file inherits no origin layer, so it defaults to the common layer instead of
/// the side that happened to run. The sync window can still retarget an entry to one side.
fn collect_added_sync_entries(
    source_root: &Path,
    include_path: &str,
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
            target: SyncTarget::Common,
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
    path.split(['/', '\\'])
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

fn find_project_java(project_dir: &Path, major: u16) -> Option<PathBuf> {
    let root = project_dir.join(format!(".mcmpu/java/{major}/runtime"));
    find_java_under(&root)
}

fn find_java_under(root: &Path) -> Option<PathBuf> {
    if !root.is_dir() {
        return None;
    }
    WalkDir::new(root)
        .follow_links(false)
        .into_iter()
        .filter_map(Result::ok)
        .find_map(|entry| {
            (entry.file_type().is_file() && entry.file_name() == java_executable_name())
                .then(|| entry.into_path())
        })
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

#[cfg(test)]
mod gradle_cache_tests {
    use std::path::PathBuf;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    /// Records every URL it is asked for and serves one fixed payload.
    struct CountingDownloader {
        payload: Vec<u8>,
        urls: Mutex<Vec<String>>,
        count: AtomicUsize,
    }

    impl CountingDownloader {
        fn new(payload: &[u8]) -> Self {
            Self {
                payload: payload.to_vec(),
                urls: Mutex::new(Vec::new()),
                count: AtomicUsize::new(0),
            }
        }

        fn count(&self) -> usize {
            self.count.load(Ordering::SeqCst)
        }
    }

    impl DownloadBytes for CountingDownloader {
        fn download_bytes(&self, url: &str) -> Result<Vec<u8>, RunError> {
            self.count.fetch_add(1, Ordering::SeqCst);
            self.urls.lock().unwrap().push(url.to_string());
            Ok(self.payload.clone())
        }
    }

    fn run_file(path: &str, bytes: &[u8], project: &Path) -> RunFile {
        RunFile {
            path: path.to_string(),
            url: format!("https://example.invalid/{path}"),
            cache_path: project.join(".mcmpu/cache").join(path),
            sha1: format!("{:x}", Sha1::digest(bytes)),
            size: bytes.len() as u64,
        }
    }

    /// Places one artifact in Gradle's cache layout and returns its path.
    fn gradle_artifact(
        home: &Path,
        module: &str,
        file_id: &str,
        name: &str,
        bytes: &[u8],
    ) -> PathBuf {
        let path = home
            .join("caches/modules-2/files-2.1/curse.maven")
            .join(module)
            .join(file_id)
            .join("0123456789abcdef")
            .join(name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, bytes).unwrap();
        path
    }

    #[test]
    fn coordinates_parse_curseforge_run_names_with_sort_prefixes() {
        assert_eq!(
            gradle_curseforge_coordinates("mods/!red-core-873867-6147102.jar"),
            Some((
                "red-core-873867".to_string(),
                "6147102".to_string(),
                "jar".to_string()
            ))
        );
        assert_eq!(
            gradle_curseforge_coordinates("resourcepacks/+++pack-1005815-8230893.zip"),
            Some((
                "pack-1005815".to_string(),
                "8230893".to_string(),
                "zip".to_string()
            ))
        );
        assert_eq!(
            gradle_curseforge_coordinates("shaderpacks/skin-layers-3d-521480-6147102.jar"),
            Some((
                "skin-layers-3d-521480".to_string(),
                "6147102".to_string(),
                "jar".to_string()
            ))
        );
        assert_eq!(gradle_curseforge_coordinates("mods/plain-name.jar"), None);
        assert_eq!(gradle_curseforge_coordinates("mods/local.jar"), None);
    }

    #[test]
    fn gradle_lookup_accepts_only_a_verified_artifact() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("gradle");
        let bytes = b"cached mod";
        let file = run_file("mods/fugue-1005815-1006815.jar", bytes, temp.path());
        gradle_artifact(
            &home,
            "fugue-1005815",
            "1006815",
            "fugue-1005815-1006815.jar",
            bytes,
        );

        let found = find_gradle_cached_run_file_at(&file, Some(&home)).expect("verified hit");
        assert_eq!(std::fs::read(&found).unwrap(), bytes);

        // A same-named artifact with different content must not satisfy the request.
        let archive = temp.path().join("other");
        gradle_artifact(
            &archive,
            "fugue-1005815",
            "1006815",
            "fugue-1005815-1006815.jar",
            b"tampered",
        );
        assert!(find_gradle_cached_run_file_at(&file, Some(&archive)).is_none());

        // An unrelated artifact name in the same module must be ignored as well.
        let wrong_name = temp.path().join("wrong-name");
        gradle_artifact(
            &wrong_name,
            "fugue-1005815",
            "1006815",
            "fugue-1005815-1006816.jar",
            bytes,
        );
        assert!(find_gradle_cached_run_file_at(&file, Some(&wrong_name)).is_none());

        assert!(find_gradle_cached_run_file_at(&file, None).is_none());
    }

    #[test]
    fn gradle_hit_publishes_to_run_directory_without_downloading() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("gradle");
        let run_dir = temp.path().join("run/client");
        std::fs::create_dir_all(&run_dir).unwrap();
        let bytes = b"cached mod";
        let file = run_file("mods/fugue-1005815-1006815.jar", bytes, temp.path());
        gradle_artifact(
            &home,
            "fugue-1005815",
            "1006815",
            "fugue-1005815-1006815.jar",
            bytes,
        );

        let downloader = CountingDownloader::new(b"downloaded");
        let files = vec![file];
        verify_or_download_run_files_at(&run_dir, &files, &downloader, Some(&home), &mut |_| {})
            .unwrap();

        assert_eq!(downloader.count(), 0);
        assert_eq!(
            std::fs::read(run_dir.join("mods/fugue-1005815-1006815.jar")).unwrap(),
            bytes
        );
        assert_eq!(std::fs::read(&files[0].cache_path).unwrap(), bytes);
    }

    #[test]
    fn damaged_gradle_copy_falls_back_to_the_downloader() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("gradle");
        let run_dir = temp.path().join("run/client");
        std::fs::create_dir_all(&run_dir).unwrap();
        let bytes = b"cached mod";
        let file = run_file("mods/fugue-1005815-1006815.jar", bytes, temp.path());
        gradle_artifact(
            &home,
            "fugue-1005815",
            "1006815",
            "fugue-1005815-1006815.jar",
            b"stale",
        );

        let downloader = CountingDownloader::new(bytes);
        let files = vec![file];
        verify_or_download_run_files_at(&run_dir, &files, &downloader, Some(&home), &mut |_| {})
            .unwrap();

        assert_eq!(downloader.count(), 1);
        assert_eq!(
            std::fs::read(run_dir.join("mods/fugue-1005815-1006815.jar")).unwrap(),
            bytes
        );
    }

    #[test]
    fn valid_run_target_is_reused_without_touching_gradle() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("gradle");
        let run_dir = temp.path().join("run/client");
        let bytes = b"cached mod";
        let file = run_file("mods/fugue-1005815-1006815.jar", bytes, temp.path());
        let target = run_dir.join(&file.path);
        std::fs::create_dir_all(target.parent().unwrap()).unwrap();
        std::fs::write(&target, bytes).unwrap();
        gradle_artifact(
            &home,
            "fugue-1005815",
            "1006815",
            "fugue-1005815-1006815.jar",
            bytes,
        );
        let cache_before = file.cache_path.exists();

        let downloader = CountingDownloader::new(b"downloaded");
        let files = vec![file];
        verify_or_download_run_files_at(&run_dir, &files, &downloader, Some(&home), &mut |_| {})
            .unwrap();

        assert_eq!(downloader.count(), 0);
        assert!(!cache_before);
        assert!(!files[0].cache_path.exists());
        assert_eq!(std::fs::read(&target).unwrap(), bytes);
    }
}

#[cfg(test)]
mod server_run_configuration_tests {
    use super::*;

    /// First run writes both files; a second run leaves them untouched.
    #[test]
    fn server_run_configuration_is_written_once_and_then_left_alone() {
        let temp = tempfile::tempdir().unwrap();

        let created = ensure_server_run_configuration(temp.path()).unwrap();
        assert_eq!(
            created,
            vec![EULA_FILE.to_string(), SERVER_PROPERTIES_FILE.to_string()]
        );
        let eula = std::fs::read_to_string(temp.path().join(EULA_FILE)).unwrap();
        assert!(eula.contains("eula=true"));
        assert!(eula.contains("https://aka.ms/MinecraftEULA"));
        assert_eq!(
            std::fs::read_to_string(temp.path().join(SERVER_PROPERTIES_FILE)).unwrap(),
            "online-mode=false
"
        );

        assert!(
            ensure_server_run_configuration(temp.path())
                .unwrap()
                .is_empty()
        );
    }

    /// A rejected EULA and a re-enabled online mode are corrected, other keys survive.
    #[test]
    fn server_run_configuration_corrects_only_the_required_keys() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(
            temp.path().join(EULA_FILE),
            "eula=false
",
        )
        .unwrap();
        std::fs::write(
            temp.path().join(SERVER_PROPERTIES_FILE),
            "# pack settings
level-name=Nova
online-mode=TRUE
max-players=20
# online-mode=false
",
        )
        .unwrap();

        ensure_server_run_configuration(temp.path()).unwrap();

        assert!(
            std::fs::read_to_string(temp.path().join(EULA_FILE))
                .unwrap()
                .contains("eula=true")
        );
        let properties = std::fs::read_to_string(temp.path().join(SERVER_PROPERTIES_FILE)).unwrap();
        assert!(properties.contains("online-mode=false"));
        assert!(!properties.contains("online-mode=TRUE"));
        assert!(properties.contains("level-name=Nova"));
        assert!(properties.contains("max-players=20"));
        assert!(properties.contains("# online-mode=false"));
    }
}
