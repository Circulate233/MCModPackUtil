use std::collections::{BTreeSet, VecDeque};
use std::ffi::OsString;
use std::fs::{self, OpenOptions};
use std::io::{BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use sha1::{Digest, Sha1};
use thiserror::Error;

use crate::config::{Config, LoaderKind};
use crate::net::{
    ArtifactRequest, ArtifactTransfer, HttpRequest, HttpTransport, NetworkEngine, NetworkError,
    TransferError,
};
use crate::project::launcher::{FileVersionJsonSource, LaunchEnvironment, LoaderLaunchResolver};
use crate::project::process::configure_console_window;

const VERSION_MANIFEST_URL: &str =
    "https://piston-meta.mojang.com/mc/game/version_manifest_v2.json";
const CLIENT_LAUNCHER_PROFILES: &str = r#"{
  "profiles": {},
  "selectedProfile": "(Default)",
  "clientToken": "00000000000000000000000000000000"
}"#;
const MAX_INSTALLER_TRANSCRIPT_LINES: usize = 2_048;
const MAX_INSTALLER_TRANSCRIPT_BYTES: usize = 256 * 1024;
const MAX_INSTALLER_TRANSCRIPT_LINE_BYTES: usize = 16 * 1024;
const INSTALLER_OUTPUT_READ_BUFFER_BYTES: usize = 8 * 1024;
const INSTALLER_OUTPUT_CHANNEL_CAPACITY: usize = 512;
const INSTALLATION_RECORD_VERSION: u32 = 1;
/// Records which loader installation the `libraries` directory currently holds.
const LIBRARIES_MARKER_NAME: &str = ".mcmpu-libraries.json";
const LIBRARIES_MARKER_VERSION: u32 = 1;
const RUNTIME_INSTALL_LOCK_TIMEOUT: Duration = Duration::from_secs(120);
const MAX_CHECKSUM_RESPONSE_BYTES: usize = 64 * 1024;
const MAX_RUNTIME_METADATA_RESPONSE_BYTES: usize = 16 * 1024 * 1024;
/// How many loader libraries are submitted as one transfer batch.
///
/// A batch only reports its results once every request in it has settled, so a single batch over
/// every library showed no output at all until the last download finished. Submitting them in
/// chunks keeps progress moving while the downloads are still running.
const INSTALLER_LIBRARY_BATCH_SIZE: usize = 8;
static CACHE_TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// Identifies the standard stream that produced an installer log line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuntimeInstallStream {
    /// The installer's standard output stream.
    Stdout,
    /// The installer's standard error stream.
    Stderr,
}

/// Incremental status emitted while preparing a loader runtime.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RuntimeInstallProgress {
    /// Installer artifact download and checksum verification is starting.
    DownloadingInstaller { url: String, target: PathBuf },
    /// The verified installer artifact is ready on disk.
    InstallerDownloaded { target: PathBuf },
    /// The installer Java process is about to start.
    RunningInstaller { loader: LoaderKind },
    /// One complete output line was received while the installer is still running.
    InstallerOutput {
        stream: RuntimeInstallStream,
        line: String,
    },
    /// The libraries the installer would download are being fetched by the project downloader.
    InstallingLibraries { total: usize },
    /// One installer library is verified and present in the run directory.
    LibraryReady {
        completed: usize,
        total: usize,
        path: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum RuntimeInstallSide {
    Client,
    Server,
}

impl RuntimeInstallSide {
    fn file_name(self) -> &'static str {
        match self {
            Self::Client => "client",
            Self::Server => "server",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct InstallerArtifactRecord {
    url: String,
    sha1: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RuntimeInstallationRecord {
    version: u32,
    side: RuntimeInstallSide,
    loader: LoaderKind,
    minecraft: String,
    loader_version: String,
    installer: Option<InstallerArtifactRecord>,
    client_profile: Option<String>,
    server_launch_output: Option<String>,
}

#[derive(Debug, Clone)]
struct InstallerProcessSpec {
    executable: PathBuf,
    args: Vec<OsString>,
    env: Vec<(OsString, OsString)>,
    working_dir: PathBuf,
    suppress_console_window: bool,
}

#[derive(Debug)]
struct InstallerProcessResult {
    success: bool,
    status: String,
    transcript: Vec<String>,
}

/// Keeps the most recent installer diagnostics without allowing a verbose installer to retain
/// unbounded process output in memory.
#[derive(Debug, Default)]
struct InstallerTranscriptTail {
    lines: VecDeque<String>,
    bytes: usize,
    discarded_lines: usize,
}

impl InstallerTranscriptTail {
    fn push(&mut self, line: String) {
        let line = truncate_utf8_tail(line, MAX_INSTALLER_TRANSCRIPT_LINE_BYTES);
        let line_bytes = line.len();
        while self.lines.len() >= MAX_INSTALLER_TRANSCRIPT_LINES
            || self.bytes.saturating_add(line_bytes) > MAX_INSTALLER_TRANSCRIPT_BYTES
        {
            let Some(discarded) = self.lines.pop_front() else {
                break;
            };
            self.bytes = self.bytes.saturating_sub(discarded.len());
            self.discarded_lines += 1;
        }
        self.bytes += line_bytes;
        self.lines.push_back(line);
    }

    fn into_lines(mut self) -> Vec<String> {
        if self.discarded_lines == 0 {
            return self.lines.into_iter().collect();
        }
        loop {
            let notice = format!(
                "[installer transcript truncated: {} earlier lines omitted]",
                self.discarded_lines
            );
            if self.lines.len() < MAX_INSTALLER_TRANSCRIPT_LINES
                && self.bytes.saturating_add(notice.len()) <= MAX_INSTALLER_TRANSCRIPT_BYTES
            {
                let mut lines = Vec::with_capacity(self.lines.len() + 1);
                lines.push(notice);
                lines.extend(self.lines);
                return lines;
            }
            let Some(discarded) = self.lines.pop_front() else {
                return vec![notice];
            };
            self.bytes = self.bytes.saturating_sub(discarded.len());
            self.discarded_lines += 1;
        }
    }
}

fn truncate_utf8_tail(mut value: String, max_bytes: usize) -> String {
    if value.len() <= max_bytes {
        return value;
    }
    let prefix = "[line truncated] ";
    let retained = max_bytes.saturating_sub(prefix.len());
    let start = value.len().saturating_sub(retained);
    let boundary = value
        .char_indices()
        .map(|(index, _)| index)
        .find(|&index| index >= start)
        .unwrap_or(value.len());
    value.replace_range(..boundary, prefix);
    value
}

trait InstallerProcessRunner: Send + Sync {
    fn run(
        &self,
        spec: &InstallerProcessSpec,
        progress: &mut dyn FnMut(RuntimeInstallStream, String),
    ) -> Result<InstallerProcessResult, InstallerProcessError>;
}

#[derive(Debug, Clone, Copy)]
struct SystemInstallerProcessRunner;

impl InstallerProcessRunner for SystemInstallerProcessRunner {
    fn run(
        &self,
        spec: &InstallerProcessSpec,
        progress: &mut dyn FnMut(RuntimeInstallStream, String),
    ) -> Result<InstallerProcessResult, InstallerProcessError> {
        let mut command = Command::new(&spec.executable);
        command
            .args(&spec.args)
            .envs(spec.env.iter().cloned())
            .current_dir(&spec.working_dir)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        configure_console_window(&mut command, spec.suppress_console_window);
        let mut child = command.spawn().map_err(InstallerProcessError::Start)?;
        let stdout = take_installer_pipe(
            child.stdout.take(),
            &mut child,
            RuntimeInstallStream::Stdout,
        )?;
        let stderr = take_installer_pipe(
            child.stderr.take(),
            &mut child,
            RuntimeInstallStream::Stderr,
        )?;
        let (sender, receiver) = mpsc::sync_channel(INSTALLER_OUTPUT_CHANNEL_CAPACITY);
        let stdout_reader =
            spawn_installer_reader(stdout, RuntimeInstallStream::Stdout, sender.clone());
        let stderr_reader = spawn_installer_reader(stderr, RuntimeInstallStream::Stderr, sender);
        let mut transcript = InstallerTranscriptTail::default();
        for (stream, line) in receiver {
            transcript.push(format!("{}: {line}", installer_stream_name(stream)));
            progress(stream, line);
        }
        let stdout_result = join_installer_reader(stdout_reader, RuntimeInstallStream::Stdout);
        let stderr_result = join_installer_reader(stderr_reader, RuntimeInstallStream::Stderr);
        let status_result = child.wait().map_err(InstallerProcessError::Wait);
        stdout_result?;
        stderr_result?;
        let status = status_result?;
        Ok(InstallerProcessResult {
            success: status.success(),
            status: status.to_string(),
            transcript: transcript.into_lines(),
        })
    }
}

fn take_installer_pipe<T>(
    pipe: Option<T>,
    child: &mut Child,
    stream: RuntimeInstallStream,
) -> Result<T, InstallerProcessError> {
    match pipe {
        Some(pipe) => Ok(pipe),
        None => {
            let _ = child.kill();
            let _ = child.wait();
            Err(InstallerProcessError::Pipe { stream })
        }
    }
}

fn spawn_installer_reader<R>(
    reader: R,
    stream: RuntimeInstallStream,
    sender: mpsc::SyncSender<(RuntimeInstallStream, String)>,
) -> thread::JoinHandle<Result<(), std::io::Error>>
where
    R: Read + Send + 'static,
{
    thread::spawn(move || {
        let mut reader = BufReader::new(reader);
        let mut read_buffer = [0_u8; INSTALLER_OUTPUT_READ_BUFFER_BYTES];
        let mut line_buffer = Vec::new();
        let mut line_truncated = false;
        loop {
            let read = reader.read(&mut read_buffer)?;
            if read == 0 {
                if !line_buffer.is_empty() || line_truncated {
                    send_installer_line(&sender, stream, &mut line_buffer, line_truncated)?;
                }
                break;
            }
            for byte in &read_buffer[..read] {
                if *byte == b'\n' {
                    send_installer_line(&sender, stream, &mut line_buffer, line_truncated)?;
                    line_truncated = false;
                } else if line_buffer.len() < MAX_INSTALLER_TRANSCRIPT_LINE_BYTES {
                    line_buffer.push(*byte);
                } else {
                    line_truncated = true;
                }
            }
        }
        Ok(())
    })
}

fn send_installer_line(
    sender: &mpsc::SyncSender<(RuntimeInstallStream, String)>,
    stream: RuntimeInstallStream,
    line_buffer: &mut Vec<u8>,
    truncated: bool,
) -> Result<(), std::io::Error> {
    while matches!(line_buffer.last(), Some(b'\r')) {
        line_buffer.pop();
    }
    let mut line = String::from_utf8_lossy(line_buffer).into_owned();
    if truncated {
        line.push_str(" [line truncated]");
    }
    line_buffer.clear();
    sender
        .send((stream, line))
        .map_err(|_| std::io::Error::other("installer output receiver closed"))
}

fn join_installer_reader(
    reader: thread::JoinHandle<Result<(), std::io::Error>>,
    stream: RuntimeInstallStream,
) -> Result<(), InstallerProcessError> {
    reader
        .join()
        .map_err(|_| InstallerProcessError::ReaderPanicked { stream })?
        .map_err(|source| InstallerProcessError::Read { stream, source })
}

fn installer_stream_name(stream: RuntimeInstallStream) -> &'static str {
    match stream {
        RuntimeInstallStream::Stdout => "stdout",
        RuntimeInstallStream::Stderr => "stderr",
    }
}

#[derive(Debug, Error)]
pub enum InstallerProcessError {
    #[error("failed to start process: {0}")]
    Start(#[source] std::io::Error),
    #[error("installer {stream:?} pipe was unavailable")]
    Pipe { stream: RuntimeInstallStream },
    #[error("failed to read installer {stream:?}: {source}")]
    Read {
        stream: RuntimeInstallStream,
        #[source]
        source: std::io::Error,
    },
    #[error("installer {stream:?} reader thread panicked")]
    ReaderPanicked { stream: RuntimeInstallStream },
    #[error("failed to wait for process: {0}")]
    Wait(#[source] std::io::Error),
}

/// Locates a concrete loader profile after a runtime installer has produced it.
pub trait RuntimeProfileLocator {
    /// Ensures the requested client profile exists beneath the run directory and returns its id.
    fn prepare_client_profile(
        &self,
        run_dir: &Path,
        config: &Config,
        java: &Path,
        progress: &mut dyn FnMut(&RuntimeInstallProgress),
    ) -> Result<String, RuntimeInstallError>;

    /// Installs a server distribution and returns the exact JVM arguments emitted by its installer.
    fn prepare_server_arguments(
        &self,
        run_dir: &Path,
        config: &Config,
        java: &Path,
        progress: &mut dyn FnMut(&RuntimeInstallProgress),
    ) -> Result<Vec<String>, RuntimeInstallError>;
}

/// HTTP and process based runtime profile installer.
pub struct RuntimeProfileLocatorImpl {
    downloader: Arc<dyn RuntimeInstallDownloader>,
    process_runner: Arc<dyn InstallerProcessRunner>,
    installer_proxy_arguments: Vec<String>,
    runtime_cache: PathBuf,
}

trait RuntimeInstallDownloader: Send + Sync {
    fn download(&self, url: &str) -> Result<Vec<u8>, RuntimeInstallError>;

    fn download_bounded(
        &self,
        url: &str,
        max_response_bytes: usize,
    ) -> Result<Vec<u8>, RuntimeInstallError> {
        let bytes = self.download(url)?;
        if bytes.len() > max_response_bytes {
            return Err(RuntimeInstallError::MetadataTooLarge {
                url: url.to_string(),
                limit: max_response_bytes,
            });
        }
        Ok(bytes)
    }

    fn download_artifact(
        &self,
        url: &str,
        target: &Path,
        expected_sha1: &str,
    ) -> Result<(), RuntimeInstallError> {
        let bytes = self.download(url)?;
        verify_installer_sha1(url, expected_sha1, &bytes)?;
        atomic_write_file(target, &bytes)
    }

    /// Fetches many artifacts through the shared transfer budget.
    ///
    /// The default keeps one-at-a-time behavior for downloaders without a batch path; the network
    /// downloader overrides it so installer libraries share the engine's parallelism.
    fn download_artifacts(
        &self,
        requests: &[InstallerLibraryRequest],
    ) -> Vec<Result<(), RuntimeInstallError>> {
        requests
            .iter()
            .map(|request| self.download_artifact(&request.url, &request.target, &request.sha1))
            .collect()
    }
}

/// One installer library the project downloader should fetch into the run directory.
#[derive(Debug, Clone)]
struct InstallerLibraryRequest {
    url: String,
    target: PathBuf,
    sha1: String,
}

struct NetworkRuntimeInstallDownloader {
    engine: Arc<NetworkEngine>,
}

impl RuntimeInstallDownloader for NetworkRuntimeInstallDownloader {
    fn download(&self, url: &str) -> Result<Vec<u8>, RuntimeInstallError> {
        self.download_bounded(url, MAX_RUNTIME_METADATA_RESPONSE_BYTES)
    }

    fn download_bounded(
        &self,
        url: &str,
        max_response_bytes: usize,
    ) -> Result<Vec<u8>, RuntimeInstallError> {
        let request = HttpRequest::get(url, max_response_bytes).build()?;
        Ok(self.engine.get_bytes(request)?.body)
    }

    fn download_artifact(
        &self,
        url: &str,
        target: &Path,
        expected_sha1: &str,
    ) -> Result<(), RuntimeInstallError> {
        let mut candidates = artifact_candidate_urls(url);
        let primary = candidates.remove(0);
        let mut builder = ArtifactRequest::builder("loader-installer", target, primary)
            .expected_sha1(expected_sha1);
        for candidate in candidates {
            builder = builder.candidate_url(candidate);
        }
        self.engine
            .transfer_one(builder.build()?, Arc::new(|_| {}))?;
        Ok(())
    }

    fn download_artifacts(
        &self,
        requests: &[InstallerLibraryRequest],
    ) -> Vec<Result<(), RuntimeInstallError>> {
        if requests.is_empty() {
            return Vec::new();
        }
        // Every task id has to be unique: the transfer layer rejects a whole batch that repeats
        // one, and these all shared a single label, so no library was ever fetched. The target
        // path identifies a library within one batch. Mirror candidates are appended after the
        // original URL so the transfer layer probes them and keeps whichever answers first.
        let built = requests
            .iter()
            .map(|request| {
                let mut candidates = artifact_candidate_urls(&request.url);
                let primary = candidates.remove(0);
                let mut builder = ArtifactRequest::builder(
                    request.target.to_string_lossy().as_ref(),
                    &request.target,
                    &primary,
                );
                for candidate in candidates {
                    builder = builder.candidate_url(candidate);
                }
                builder.expected_sha1(&request.sha1).build()
            })
            .collect::<Result<Vec<_>, _>>();
        let built = match built {
            Ok(built) => built,
            Err(error) => {
                let reason = error.to_string();
                return requests
                    .iter()
                    .map(|_| {
                        Err(RuntimeInstallError::InstallerLibraryBatch {
                            reason: reason.clone(),
                        })
                    })
                    .collect();
            }
        };
        self.engine
            .transfer_many(built, Arc::new(|_| {}))
            .into_iter()
            .map(|result| result.map(|_| ()).map_err(RuntimeInstallError::from))
            .collect()
    }
}

impl RuntimeProfileLocatorImpl {
    /// Creates a profile installer rooted at the project's persistent runtime cache.
    ///
    /// Metadata, installer artifacts, and validated installation records are shared by subsequent
    /// client and server runs. The optional proxy is applied to both Rust HTTP requests and the
    /// loader installer's JVM without being written to cache records or progress messages.
    pub fn new(
        runtime_cache: impl Into<PathBuf>,
        proxy: Option<&str>,
    ) -> Result<Self, RuntimeInstallError> {
        let proxy = proxy.map(str::trim).filter(|proxy| !proxy.is_empty());
        let installer_proxy_arguments = installer_jvm_proxy_arguments(proxy)?;
        let engine = Arc::new(NetworkEngine::new(crate::net::NetworkConfig {
            proxy: proxy.map(str::to_owned),
            ..crate::net::NetworkConfig::default()
        })?);
        Ok(Self::with_network_engine_and_proxy_arguments(
            runtime_cache,
            engine,
            installer_proxy_arguments,
        ))
    }

    /// Creates a runtime installer that shares the caller's request budgets and connection pool.
    pub fn with_network_engine(
        runtime_cache: impl Into<PathBuf>,
        proxy: Option<&str>,
        engine: Arc<NetworkEngine>,
    ) -> Result<Self, RuntimeInstallError> {
        let proxy = proxy.map(str::trim).filter(|proxy| !proxy.is_empty());
        let installer_proxy_arguments = installer_jvm_proxy_arguments(proxy)?;
        Ok(Self::with_network_engine_and_proxy_arguments(
            runtime_cache,
            engine,
            installer_proxy_arguments,
        ))
    }

    fn with_network_engine_and_proxy_arguments(
        runtime_cache: impl Into<PathBuf>,
        engine: Arc<NetworkEngine>,
        installer_proxy_arguments: Vec<String>,
    ) -> Self {
        Self {
            downloader: Arc::new(NetworkRuntimeInstallDownloader { engine }),
            process_runner: Arc::new(SystemInstallerProcessRunner),
            installer_proxy_arguments,
            runtime_cache: runtime_cache.into(),
        }
    }

    fn installer_arguments<I>(&self, installer_arguments: I) -> Vec<OsString>
    where
        I: IntoIterator<Item = OsString>,
    {
        self.installer_proxy_arguments
            .iter()
            .map(OsString::from)
            .chain(installer_arguments)
            .collect()
    }

    fn download(
        &self,
        url: &str,
        max_response_bytes: usize,
    ) -> Result<Vec<u8>, RuntimeInstallError> {
        self.downloader.download_bounded(url, max_response_bytes)
    }

    fn download_verified_installer(
        &self,
        installer_url: &str,
        declared_sha1: Option<&str>,
        progress: &mut dyn FnMut(&RuntimeInstallProgress),
    ) -> Result<(PathBuf, InstallerArtifactRecord), RuntimeInstallError> {
        let checksum_url = format!("{installer_url}.sha1");
        let expected_sha1 = match declared_sha1 {
            Some(value) => parse_sha1(value, "loader plan")?,
            None => self.resolve_installer_checksum(&checksum_url, progress)?,
        };
        let target = self
            .runtime_cache
            .join("installers")
            .join(format!("{expected_sha1}.jar"));
        if valid_sha1_file(&target, &expected_sha1)? {
            emit_cache_hit(progress, "loader installer", &target);
            return Ok((
                target,
                InstallerArtifactRecord {
                    url: installer_url.to_string(),
                    sha1: expected_sha1,
                },
            ));
        }
        remove_invalid_cache_file(&target)?;
        progress(&RuntimeInstallProgress::DownloadingInstaller {
            url: installer_url.to_string(),
            target: target.clone(),
        });
        self.downloader
            .download_artifact(installer_url, &target, &expected_sha1)?;
        progress(&RuntimeInstallProgress::InstallerDownloaded {
            target: target.clone(),
        });
        Ok((
            target,
            InstallerArtifactRecord {
                url: installer_url.to_string(),
                sha1: expected_sha1,
            },
        ))
    }

    fn resolve_installer_checksum(
        &self,
        checksum_url: &str,
        progress: &mut dyn FnMut(&RuntimeInstallProgress),
    ) -> Result<String, RuntimeInstallError> {
        let url_key = format!("{:x}", Sha1::digest(checksum_url.as_bytes()));
        let target = self
            .runtime_cache
            .join("installers/checksums")
            .join(format!("{url_key}.sha1"));
        if let Ok(bytes) = fs::read(&target)
            && let Ok(checksum) = parse_checksum_sidecar(checksum_url, &bytes)
        {
            emit_cache_hit(progress, "loader installer checksum", &target);
            return Ok(checksum);
        }
        remove_invalid_cache_file(&target)?;
        let bytes = self.download(checksum_url, MAX_CHECKSUM_RESPONSE_BYTES)?;
        let checksum = parse_checksum_sidecar(checksum_url, &bytes)?;
        atomic_write_file(&target, format!("{checksum}\n").as_bytes())?;
        Ok(checksum)
    }

    fn ensure_vanilla_profile(
        &self,
        run_dir: &Path,
        minecraft: &str,
        progress: &mut dyn FnMut(&RuntimeInstallProgress),
    ) -> Result<(), RuntimeInstallError> {
        let target = profile_path(run_dir, minecraft);
        if valid_profile_id_file(&target, minecraft)? {
            emit_cache_hit(progress, "Minecraft version profile", &target);
            return Ok(());
        }
        remove_invalid_cache_file(&target)?;
        let cached_profile = self
            .runtime_cache
            .join("metadata/versions")
            .join(format!("{}.json", safe_cache_component(minecraft)));
        if valid_profile_id_file(&cached_profile, minecraft)? {
            copy_file_atomically(&cached_profile, &target)?;
            emit_cache_hit(
                progress,
                "cached Minecraft version profile",
                &cached_profile,
            );
            return Ok(());
        }
        remove_invalid_cache_file(&cached_profile)?;
        let manifest_path = self.runtime_cache.join("metadata/version_manifest_v2.json");
        let manifest = match read_valid_version_manifest(&manifest_path) {
            Ok(Some(manifest)) => {
                emit_cache_hit(progress, "Mojang version manifest", &manifest_path);
                manifest
            }
            Ok(None) => {
                remove_invalid_cache_file(&manifest_path)?;
                let bytes =
                    self.download(VERSION_MANIFEST_URL, MAX_RUNTIME_METADATA_RESPONSE_BYTES)?;
                let manifest = parse_version_manifest(&bytes)?;
                atomic_write_file(&manifest_path, &bytes)?;
                manifest
            }
            Err(error) => return Err(error),
        };
        let entry = manifest
            .versions
            .into_iter()
            .find(|entry| entry.id == minecraft)
            .ok_or_else(|| RuntimeInstallError::MinecraftVersionMissing {
                version: minecraft.to_string(),
            })?;
        let bytes = self.download(&entry.url, MAX_RUNTIME_METADATA_RESPONSE_BYTES)?;
        validate_profile_id_bytes(&bytes, minecraft)?;
        atomic_write_file(&cached_profile, &bytes)?;
        atomic_write_file(&target, &bytes)
    }

    fn install_fabric(
        &self,
        run_dir: &Path,
        config: &Config,
        progress: &mut dyn FnMut(&RuntimeInstallProgress),
    ) -> Result<String, RuntimeInstallError> {
        let profile_url = format!(
            "https://meta.fabricmc.net/v2/versions/loader/{}/{}/profile/json",
            config.pack.minecraft_version, config.pack.loader_version
        );
        let cached_profile = self.runtime_cache.join("metadata/fabric").join(format!(
            "{}-{}.json",
            safe_cache_component(&config.pack.minecraft_version),
            safe_cache_component(&config.pack.loader_version)
        ));
        if let Some(profile) = read_valid_loader_profile(
            &cached_profile,
            &LoaderKind::Fabric,
            &config.pack.minecraft_version,
            &config.pack.loader_version,
            None,
        )? {
            let target = profile_path(run_dir, &profile.id);
            copy_file_atomically(&cached_profile, &target)?;
            emit_cache_hit(progress, "Fabric loader profile", &cached_profile);
            return Ok(profile.id);
        }
        remove_invalid_cache_file(&cached_profile)?;
        let bytes = self.download(&profile_url, MAX_RUNTIME_METADATA_RESPONSE_BYTES)?;
        let profile = validate_loader_profile_bytes(
            &bytes,
            &LoaderKind::Fabric,
            &config.pack.minecraft_version,
            &config.pack.loader_version,
            None,
        )?;
        atomic_write_file(&cached_profile, &bytes)?;
        atomic_write_file(&profile_path(run_dir, &profile.id), &bytes)?;
        Ok(profile.id)
    }

    fn install_jar_profile(
        &self,
        run_dir: &Path,
        config: &Config,
        java: &Path,
        progress: &mut dyn FnMut(&RuntimeInstallProgress),
    ) -> Result<String, RuntimeInstallError> {
        let plan = crate::server::loader::plan_loader_install(
            &config.pack.loader,
            &config.pack.minecraft_version,
            &config.pack.loader_version,
        )
        .map_err(|source| RuntimeInstallError::LoaderPlan { source })?;
        let (url, sha1) = match plan {
            crate::server::loader::LoaderInstallerPlan::Forge {
                installer_url,
                installer_sha1,
            }
            | crate::server::loader::LoaderInstallerPlan::NeoForge {
                installer_url,
                installer_sha1,
            }
            | crate::server::loader::LoaderInstallerPlan::Cleanroom {
                installer_url,
                installer_sha1,
            } => (installer_url, installer_sha1),
            crate::server::loader::LoaderInstallerPlan::Fabric { .. } => {
                unreachable!("Fabric is handled by its profile API")
            }
        };
        if let Some(profile_id) = self.reusable_client_installation(
            run_dir,
            config,
            Some(&url),
            sha1.as_deref(),
            progress,
        )? {
            return Ok(profile_id);
        }
        let (installer, installer_record) =
            self.download_verified_installer(&url, sha1.as_deref(), progress)?;
        let installer_path = absolute_installer_path(&installer)?;
        // Cleanroom ships its finished launcher profile inside the installer jar, and the
        // installer writes exactly that document out. Writing it here produces a byte-identical
        // result without starting a JVM, which also avoids the installer's own downloads: the
        // intermediate files it fetches on the way (mcp_config and friends) are not part of the
        // profile at all and are the ones that fail on flaky mirrors.
        if config.pack.loader == LoaderKind::Cleanroom
            && let Some(profile_id) = self.write_cleanroom_profile_from_installer(
                run_dir,
                config,
                &installer_path,
                progress,
            )?
        {
            // The recorded installation is what makes the next start skip all of this. Returning
            // before writing it left the run directory prepared but unrecorded, so every launch
            // re-ran the whole preparation and re-verified every library.
            self.write_installation_record(&RuntimeInstallationRecord {
                version: INSTALLATION_RECORD_VERSION,
                side: RuntimeInstallSide::Client,
                loader: config.pack.loader.clone(),
                minecraft: config.pack.minecraft_version.clone(),
                loader_version: config.pack.loader_version.clone(),
                installer: Some(installer_record),
                client_profile: Some(profile_id.clone()),
                server_launch_output: None,
            })?;
            // The marker names the loader that owns `libraries/`; without it the next start treats
            // the directory as foreign and clears it, discarding every library just downloaded.
            write_libraries_marker(run_dir, config)?;
            return Ok(profile_id);
        }
        // The installer downloads its libraries one at a time; fetching them here first lets the
        // shared parallel downloader do that work and leaves the installer with nothing to fetch.
        // A failure here is not fatal, but it must be visible: the installer then does all of the
        // downloading itself, which is what the warnings below explain.
        if let Err(error) = preseed_installer_libraries(
            self.downloader.as_ref(),
            run_dir,
            &installer_path,
            &config.pack.minecraft_version,
            progress,
        ) {
            warn_preseed_skipped(progress, &installer_path, &error);
        }
        let target_path = absolute_installer_target(run_dir)?;
        write_client_launcher_profiles(&target_path)?;
        let spec = InstallerProcessSpec {
            executable: java.to_path_buf(),
            args: self.installer_arguments(client_installer_arguments(
                &config.pack.loader,
                &installer_path,
                &target_path,
            )?),
            working_dir: target_path,
            env: Vec::new(),
            suppress_console_window: true,
        };
        self.run_installer(&spec, &installer_path, &config.pack.loader, progress)?;
        let profile_id = find_loader_profile(
            run_dir,
            &config.pack.loader,
            &config.pack.minecraft_version,
            &config.pack.loader_version,
        )?;
        validate_client_profile(
            run_dir,
            &config.pack.loader,
            &profile_id,
            &config.pack.minecraft_version,
            &config.pack.loader_version,
        )?;
        self.write_installation_record(&RuntimeInstallationRecord {
            version: INSTALLATION_RECORD_VERSION,
            side: RuntimeInstallSide::Client,
            loader: config.pack.loader.clone(),
            minecraft: config.pack.minecraft_version.clone(),
            loader_version: config.pack.loader_version.clone(),
            installer: Some(installer_record),
            client_profile: Some(profile_id.clone()),
            server_launch_output: None,
        })?;
        Ok(profile_id)
    }

    fn install_server(
        &self,
        run_dir: &Path,
        config: &Config,
        java: &Path,
        progress: &mut dyn FnMut(&RuntimeInstallProgress),
    ) -> Result<(), RuntimeInstallError> {
        let plan = crate::server::loader::plan_loader_install(
            &config.pack.loader,
            &config.pack.minecraft_version,
            &config.pack.loader_version,
        )
        .map_err(|source| RuntimeInstallError::LoaderPlan { source })?;
        let (installer_url, installer_sha1) = installer_identity(&plan);
        reset_loader_directories(run_dir, config, progress)?;
        if self
            .reusable_server_installation(run_dir, config, installer_url, installer_sha1, progress)?
            .is_some()
        {
            write_libraries_marker(run_dir, config)?;
            return Ok(());
        }
        let installed_artifact = match plan {
            crate::server::loader::LoaderInstallerPlan::Fabric {
                installer_url,
                minecraft_version,
                loader_version,
                ..
            } => {
                let (installer, record) =
                    self.download_verified_installer(&installer_url, None, progress)?;
                let installer_path = absolute_installer_path(&installer)?;
                let spec = InstallerProcessSpec {
                    executable: java.to_path_buf(),
                    args: self.installer_arguments([
                        OsString::from("-jar"),
                        installer_path.as_os_str().to_owned(),
                        OsString::from("server"),
                        OsString::from("-mcversion"),
                        OsString::from(minecraft_version),
                        OsString::from("-loader"),
                        OsString::from(loader_version),
                        OsString::from("-downloadMinecraft"),
                    ]),
                    working_dir: run_dir.to_path_buf(),
                    env: Vec::new(),
                    suppress_console_window: true,
                };
                self.run_installer(&spec, &installer_path, &config.pack.loader, progress)?;
                record
            }
            crate::server::loader::LoaderInstallerPlan::Forge {
                installer_url,
                installer_sha1,
            }
            | crate::server::loader::LoaderInstallerPlan::NeoForge {
                installer_url,
                installer_sha1,
            }
            | crate::server::loader::LoaderInstallerPlan::Cleanroom {
                installer_url,
                installer_sha1,
            } => {
                let (installer, record) = self.download_verified_installer(
                    &installer_url,
                    installer_sha1.as_deref(),
                    progress,
                )?;
                let installer_path = absolute_installer_path(&installer)?;
                // Same as the client path: the libraries go in through the parallel downloader so
                // the installer finds them present instead of fetching each one itself.
                if let Err(error) = preseed_installer_libraries(
                    self.downloader.as_ref(),
                    run_dir,
                    &installer_path,
                    &config.pack.minecraft_version,
                    progress,
                ) {
                    warn_preseed_skipped(progress, &installer_path, &error);
                }
                let spec = InstallerProcessSpec {
                    executable: java.to_path_buf(),
                    args: self.installer_arguments(vec![
                        OsString::from("-jar"),
                        installer_path.as_os_str().to_owned(),
                        OsString::from("--installServer"),
                    ]),
                    working_dir: run_dir.to_path_buf(),
                    env: Vec::new(),
                    suppress_console_window: true,
                };
                self.run_installer(&spec, &installer_path, &config.pack.loader, progress)?;
                record
            }
        };
        let output = resolve_server_launch_output(run_dir, config)?;
        validate_server_launch_output(&config.pack.loader, &output)?;
        let relative_output = relative_run_path(run_dir, &output)?;
        self.write_installation_record(&RuntimeInstallationRecord {
            version: INSTALLATION_RECORD_VERSION,
            side: RuntimeInstallSide::Server,
            loader: config.pack.loader.clone(),
            minecraft: config.pack.minecraft_version.clone(),
            loader_version: config.pack.loader_version.clone(),
            installer: Some(installed_artifact),
            client_profile: None,
            server_launch_output: Some(relative_output),
        })?;
        write_libraries_marker(run_dir, config)
    }

    fn reusable_client_installation(
        &self,
        run_dir: &Path,
        config: &Config,
        installer_url: Option<&str>,
        declared_sha1: Option<&str>,
        progress: &mut dyn FnMut(&RuntimeInstallProgress),
    ) -> Result<Option<String>, RuntimeInstallError> {
        let Some(record) = self.read_installation_record(RuntimeInstallSide::Client)? else {
            return Ok(None);
        };
        if !record.matches(
            config,
            RuntimeInstallSide::Client,
            installer_url,
            declared_sha1,
        )? {
            return Ok(None);
        }
        let Some(profile_id) = record.client_profile.as_deref() else {
            return Ok(None);
        };
        if validate_client_profile(
            run_dir,
            &config.pack.loader,
            profile_id,
            &config.pack.minecraft_version,
            &config.pack.loader_version,
        )
        .is_err()
        {
            return Ok(None);
        }
        // A reused installation never runs the installer, so the libraries the installer ships
        // inside its own jar have to be restored here. Without this a cleared run directory
        // leaves them missing, and the launcher then looks for an equally named artifact in a
        // repository, where the file is a different build.
        //
        // Losing the cached installer means those bundled files cannot be restored, so the reuse
        // is declined and a full install runs instead of leaving the launch to fail later.
        match self.cached_installer_for(&record)? {
            Some(installer) => {
                emit_cache_hit(
                    progress,
                    "validated client loader installation",
                    &self.installation_record_path(RuntimeInstallSide::Client),
                );
                restore_installer_bundled_libraries(
                    &installer,
                    run_dir,
                    &config.pack.loader,
                    progress,
                )?;
            }
            None if record.installer.is_some() => return Ok(None),
            None => {
                emit_cache_hit(
                    progress,
                    "validated client loader installation",
                    &self.installation_record_path(RuntimeInstallSide::Client),
                );
            }
        }
        Ok(Some(profile_id.to_string()))
    }

    /// Locates the installer jar a recorded installation was made from.
    ///
    /// Returns `None` when the record names no installer or the cache no longer holds it; the
    /// caller then simply proceeds, since the installer path is the only one that ships bundled
    /// libraries.
    fn cached_installer_for(
        &self,
        record: &RuntimeInstallationRecord,
    ) -> Result<Option<PathBuf>, RuntimeInstallError> {
        let Some(installer) = record.installer.as_ref() else {
            return Ok(None);
        };
        let path = self
            .runtime_cache
            .join("installers")
            .join(format!("{}.jar", installer.sha1));
        if !valid_sha1_file(&path, &installer.sha1)? {
            return Ok(None);
        }
        Ok(Some(path))
    }

    fn reusable_server_installation(
        &self,
        run_dir: &Path,
        config: &Config,
        installer_url: &str,
        declared_sha1: Option<&str>,
        progress: &mut dyn FnMut(&RuntimeInstallProgress),
    ) -> Result<Option<PathBuf>, RuntimeInstallError> {
        let Some(record) = self.read_installation_record(RuntimeInstallSide::Server)? else {
            return Ok(None);
        };
        if !record.matches(
            config,
            RuntimeInstallSide::Server,
            Some(installer_url),
            declared_sha1,
        )? {
            return Ok(None);
        }
        let Some(relative_output) = record.server_launch_output.as_deref() else {
            return Ok(None);
        };
        let Ok(output) = safe_recorded_output_path(run_dir, relative_output) else {
            return Ok(None);
        };
        if validate_server_launch_output(&config.pack.loader, &output).is_err() {
            return Ok(None);
        }
        emit_cache_hit(
            progress,
            "validated server loader installation",
            &self.installation_record_path(RuntimeInstallSide::Server),
        );
        Ok(Some(output))
    }

    fn installation_record_path(&self, side: RuntimeInstallSide) -> PathBuf {
        self.runtime_cache
            .join("installations")
            .join(format!("{}.json", side.file_name()))
    }

    fn read_installation_record(
        &self,
        side: RuntimeInstallSide,
    ) -> Result<Option<RuntimeInstallationRecord>, RuntimeInstallError> {
        let path = self.installation_record_path(side);
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(source) => {
                return Err(RuntimeInstallError::Io {
                    path: path.display().to_string(),
                    source,
                });
            }
        };
        match serde_json::from_slice(&bytes) {
            Ok(record) => Ok(Some(record)),
            Err(_) => {
                remove_invalid_cache_file(&path)?;
                Ok(None)
            }
        }
    }

    fn write_installation_record(
        &self,
        record: &RuntimeInstallationRecord,
    ) -> Result<(), RuntimeInstallError> {
        let mut bytes = serde_json::to_vec_pretty(record)?;
        bytes.push(b'\n');
        atomic_write_file(&self.installation_record_path(record.side), &bytes)
    }

    fn run_installer(
        &self,
        spec: &InstallerProcessSpec,
        installer_path: &Path,
        loader: &LoaderKind,
        progress: &mut dyn FnMut(&RuntimeInstallProgress),
    ) -> Result<(), RuntimeInstallError> {
        progress(&RuntimeInstallProgress::RunningInstaller {
            loader: loader.clone(),
        });
        let result = self
            .process_runner
            .run(spec, &mut |stream, line| {
                progress(&RuntimeInstallProgress::InstallerOutput { stream, line });
            })
            .map_err(|source| RuntimeInstallError::InstallerProcess {
                path: installer_path.to_path_buf(),
                source,
            })?;
        if result.success {
            return Ok(());
        }
        Err(RuntimeInstallError::InstallerFailed {
            path: installer_path.to_path_buf(),
            details: installer_failure_details(&result.status, &result.transcript),
        })
    }

    /// Publishes the Cleanroom client profile straight from the installer's embedded metadata.
    ///
    /// The installer's `version.json` is the finished profile — running the installer only copies
    /// it out, so writing it here is equivalent while skipping both the JVM start and the
    /// installer's own downloads. Those downloads are the fragile part: its intermediate files
    /// (notably `mcp_config`, which has no checksum and is therefore never reused) are absent from
    /// the profile and fail the whole install when their mirror is slow.
    ///
    /// Returns `None` when the installer does not carry a usable profile, so the caller falls back
    /// to running it.
    fn write_cleanroom_profile_from_installer(
        &self,
        run_dir: &Path,
        config: &Config,
        installer: &Path,
        progress: &mut dyn FnMut(&RuntimeInstallProgress),
    ) -> Result<Option<String>, RuntimeInstallError> {
        let Ok(bytes) = read_installer_version_json(installer) else {
            return Ok(None);
        };
        let Ok(profile) = validate_loader_profile_bytes(
            &bytes,
            &config.pack.loader,
            &config.pack.minecraft_version,
            &config.pack.loader_version,
            None,
        ) else {
            return Ok(None);
        };
        let profile_id = profile.id.clone();
        // Only publish a profile this build can actually launch; a mismatch means the embedded
        // document is not the one this project asked for, and the installer should decide.
        if validate_cleanroom_profile_libraries(&bytes, &config.pack.loader_version).is_err() {
            return Ok(None);
        }
        // The installer keeps the game jar in the vanilla version slot. The embedded profile names
        // no path, so the resolver would place it under the profile id instead and fetch the same
        // 10 MB again. Pointing the entry at the vanilla slot keeps the launch layout the
        // installer produces, so an existing jar is reused rather than downloaded twice.
        let bytes = match anchor_client_jar_to_vanilla(&bytes, &config.pack.minecraft_version) {
            Ok(Some(anchored)) => anchored,
            Ok(None) => bytes,
            Err(_) => return Ok(None),
        };
        // The Cleanroom jar itself is still an installer-bundled library: its profile entry has an
        // empty URL and the archived copy is the only source. Running the installer used to unpack
        // it, so the fast path has to restore it here — otherwise the launcher resolves the entry
        // to a repository URL and downloads a different build of the same name.
        if let Err(error) =
            restore_installer_bundled_libraries(installer, run_dir, &config.pack.loader, progress)
        {
            warn_preseed_skipped(progress, installer, &error);
        }
        write_file(&profile_path(run_dir, &profile_id), &bytes)?;
        progress(&RuntimeInstallProgress::InstallerOutput {
            stream: RuntimeInstallStream::Stdout,
            line: format!(
                "publishing Cleanroom profile '{profile_id}' from the installer metadata; the installer is not run"
            ),
        });
        Ok(Some(profile_id))
    }
}

/// Points a profile's client jar at the vanilla version slot the installer uses.
///
/// Returns `Ok(None)` when the document already names a path or has no client download, so the
/// caller keeps the original bytes. The download URL and checksum are preserved, so the entry
/// still verifies exactly as before.
fn anchor_client_jar_to_vanilla(
    bytes: &[u8],
    minecraft: &str,
) -> Result<Option<Vec<u8>>, RuntimeInstallError> {
    let mut document: serde_json::Value =
        serde_json::from_slice(bytes).map_err(RuntimeInstallError::Json)?;
    let Some(client) = document
        .get_mut("downloads")
        .and_then(|downloads| downloads.get_mut("client"))
        .and_then(|client| client.as_object_mut())
    else {
        return Ok(None);
    };
    if client
        .get("path")
        .and_then(|path| path.as_str())
        .is_some_and(|path| !path.trim().is_empty())
    {
        return Ok(None);
    }
    client.insert(
        "path".to_string(),
        serde_json::Value::String(format!("versions/{minecraft}/{minecraft}.jar")),
    );
    serde_json::to_vec_pretty(&document)
        .map(Some)
        .map_err(RuntimeInstallError::Json)
}

impl RuntimeProfileLocator for RuntimeProfileLocatorImpl {
    fn prepare_client_profile(
        &self,
        run_dir: &Path,
        config: &Config,
        java: &Path,
        progress: &mut dyn FnMut(&RuntimeInstallProgress),
    ) -> Result<String, RuntimeInstallError> {
        let _lock = RuntimeInstallLock::acquire(
            &self.runtime_cache.join("runtime-install.lock"),
            RUNTIME_INSTALL_LOCK_TIMEOUT,
        )?;
        fs::create_dir_all(run_dir.join("versions")).map_err(|source| RuntimeInstallError::Io {
            path: run_dir.display().to_string(),
            source,
        })?;
        reset_loader_directories(run_dir, config, progress)?;
        self.ensure_vanilla_profile(run_dir, &config.pack.minecraft_version, progress)?;
        let profile_id = match config.pack.loader {
            LoaderKind::Fabric => {
                match self.reusable_client_installation(run_dir, config, None, None, progress)? {
                    Some(profile_id) => profile_id,
                    None => {
                        let profile_id = self.install_fabric(run_dir, config, progress)?;
                        validate_client_profile(
                            run_dir,
                            &config.pack.loader,
                            &profile_id,
                            &config.pack.minecraft_version,
                            &config.pack.loader_version,
                        )?;
                        self.write_installation_record(&RuntimeInstallationRecord {
                            version: INSTALLATION_RECORD_VERSION,
                            side: RuntimeInstallSide::Client,
                            loader: config.pack.loader.clone(),
                            minecraft: config.pack.minecraft_version.clone(),
                            loader_version: config.pack.loader_version.clone(),
                            installer: None,
                            client_profile: Some(profile_id.clone()),
                            server_launch_output: None,
                        })?;
                        profile_id
                    }
                }
            }
            LoaderKind::Forge | LoaderKind::NeoForge | LoaderKind::Cleanroom => {
                self.install_jar_profile(run_dir, config, java, progress)?
            }
        };
        write_libraries_marker(run_dir, config)?;
        Ok(profile_id)
    }

    fn prepare_server_arguments(
        &self,
        run_dir: &Path,
        config: &Config,
        java: &Path,
        progress: &mut dyn FnMut(&RuntimeInstallProgress),
    ) -> Result<Vec<String>, RuntimeInstallError> {
        let _lock = RuntimeInstallLock::acquire(
            &self.runtime_cache.join("runtime-install.lock"),
            RUNTIME_INSTALL_LOCK_TIMEOUT,
        )?;
        self.install_server(run_dir, config, java, progress)?;
        let args_file = resolve_server_launch_output(run_dir, config)?;
        validate_server_launch_output(&config.pack.loader, &args_file)?;
        let mut args = server_jvm_arguments(&config.pack.loader, run_dir, &args_file)?;
        args.push("nogui".to_string());
        Ok(args)
    }
}

/// Returns the exact loader installer output used as the server launch input.
pub fn server_launch_output_path(
    run_dir: &Path,
    loader: &LoaderKind,
    minecraft_version: &str,
    loader_version: &str,
    windows: bool,
) -> PathBuf {
    let args_name = if windows {
        "win_args.txt"
    } else {
        "unix_args.txt"
    };
    match loader {
        LoaderKind::Forge => run_dir
            .join("libraries/net/minecraftforge/forge")
            .join(format!("{minecraft_version}-{loader_version}"))
            .join(args_name),
        LoaderKind::NeoForge => run_dir
            .join("libraries/net/neoforged/neoforge")
            .join(loader_version)
            .join(args_name),
        LoaderKind::Fabric => run_dir.join("fabric-server-launch.jar"),
        LoaderKind::Cleanroom => cleanroom_server_jar_path(run_dir, loader_version),
    }
}

/// Turns an installer output path into direct Java arguments without a shell launcher.
/// Builds the loader arguments that start a server from its installer output.
///
/// The JVM runs with the run directory as its working directory, so the output path has to be
/// expressed relative to `run_dir`. Handing the JVM a path that is relative to the project root
/// (as `mcmpu runServer` sees it) resolves against the working directory instead and the JVM
/// fails with "Unable to access jarfile".
pub fn server_jvm_arguments(
    loader: &LoaderKind,
    run_dir: &Path,
    output: &Path,
) -> Result<Vec<String>, RuntimeInstallError> {
    let relative = output.strip_prefix(run_dir).map_err(|_| {
        RuntimeInstallError::ServerLaunchOutputOutsideRunDirectory {
            output: output.to_path_buf(),
            run_dir: run_dir.to_path_buf(),
        }
    })?;
    if relative.as_os_str().is_empty() {
        return Err(RuntimeInstallError::ServerLaunchOutputOutsideRunDirectory {
            output: output.to_path_buf(),
            run_dir: run_dir.to_path_buf(),
        });
    }
    // Use forward slashes so the argument is identical on every platform; `@` argument files and
    // `-jar` both accept them, and the recorded path stays comparable across hosts.
    let relative = relative.to_string_lossy().replace('\\', "/");
    Ok(match loader {
        LoaderKind::Forge | LoaderKind::NeoForge => vec![format!("@{relative}")],
        LoaderKind::Fabric | LoaderKind::Cleanroom => vec!["-jar".to_string(), relative],
    })
}

/// Returns the exact server launcher file written by the Cleanroom installer profile.
pub fn cleanroom_server_jar_path(run_dir: &Path, loader_version: &str) -> PathBuf {
    run_dir.join(format!("cleanroom-{loader_version}.jar"))
}

fn installer_identity(plan: &crate::server::loader::LoaderInstallerPlan) -> (&str, Option<&str>) {
    match plan {
        crate::server::loader::LoaderInstallerPlan::Forge {
            installer_url,
            installer_sha1,
        }
        | crate::server::loader::LoaderInstallerPlan::NeoForge {
            installer_url,
            installer_sha1,
        }
        | crate::server::loader::LoaderInstallerPlan::Cleanroom {
            installer_url,
            installer_sha1,
        } => (installer_url, installer_sha1.as_deref()),
        crate::server::loader::LoaderInstallerPlan::Fabric { installer_url, .. } => {
            (installer_url, None)
        }
    }
}

impl RuntimeInstallationRecord {
    fn matches(
        &self,
        config: &Config,
        side: RuntimeInstallSide,
        installer_url: Option<&str>,
        declared_sha1: Option<&str>,
    ) -> Result<bool, RuntimeInstallError> {
        if self.version != INSTALLATION_RECORD_VERSION
            || self.side != side
            || self.loader != config.pack.loader
            || self.minecraft != config.pack.minecraft_version
            || self.loader_version != config.pack.loader_version
        {
            return Ok(false);
        }
        match (installer_url, &self.installer) {
            (None, None) => Ok(true),
            (Some(url), Some(installer)) if installer.url == url => {
                let Ok(recorded_sha1) = parse_sha1(&installer.sha1, "installation record") else {
                    return Ok(false);
                };
                match declared_sha1 {
                    Some(declared) => Ok(recorded_sha1 == parse_sha1(declared, "loader plan")?),
                    None => Ok(true),
                }
            }
            _ => Ok(false),
        }
    }
}

fn resolve_server_launch_output(
    run_dir: &Path,
    config: &Config,
) -> Result<PathBuf, RuntimeInstallError> {
    if config.pack.loader != LoaderKind::Cleanroom {
        return Ok(server_launch_output_path(
            run_dir,
            &config.pack.loader,
            &config.pack.minecraft_version,
            &config.pack.loader_version,
            cfg!(windows),
        ));
    }
    let expected = cleanroom_server_jar_path(run_dir, &config.pack.loader_version);
    if expected.is_file() && verify_cleanroom_server_jar(&expected).is_ok() {
        return Ok(expected);
    }
    find_cleanroom_server_jar(run_dir)
}

fn validate_server_launch_output(
    loader: &LoaderKind,
    path: &Path,
) -> Result<(), RuntimeInstallError> {
    if !path.is_file() {
        return Err(RuntimeInstallError::ServerLaunchOutputMissing {
            path: path.to_path_buf(),
        });
    }
    match loader {
        LoaderKind::Forge | LoaderKind::NeoForge => {
            let content = fs::read_to_string(path).map_err(|source| RuntimeInstallError::Io {
                path: path.display().to_string(),
                source,
            })?;
            if content.split_whitespace().next().is_none() {
                return Err(RuntimeInstallError::InvalidServerLaunchOutput {
                    path: path.to_path_buf(),
                    reason: "argument file is empty".to_string(),
                });
            }
            validate_argument_file_libraries(path, &content)?;
            Ok(())
        }
        LoaderKind::Fabric => verify_executable_server_jar(path),
        LoaderKind::Cleanroom => verify_cleanroom_server_jar(path),
    }
}

fn validate_argument_file_libraries(
    argument_file: &Path,
    content: &str,
) -> Result<(), RuntimeInstallError> {
    let run_dir = argument_file
        .ancestors()
        .find(|ancestor| ancestor.file_name().is_some_and(|name| name == "libraries"))
        .and_then(Path::parent)
        .ok_or_else(|| RuntimeInstallError::InvalidServerLaunchOutput {
            path: argument_file.to_path_buf(),
            reason: "argument file is not located beneath the run libraries directory".to_string(),
        })?;
    for reference in library_references(content) {
        let normalized = reference.replace('\\', "/");
        let relative = Path::new(&normalized);
        if relative.is_absolute()
            || relative
                .components()
                .any(|component| matches!(component, std::path::Component::ParentDir))
        {
            return Err(RuntimeInstallError::InvalidServerLaunchOutput {
                path: argument_file.to_path_buf(),
                reason: format!("argument file contains unsafe library reference '{reference}'"),
            });
        }
        let referenced = run_dir.join(relative);
        if !referenced.is_file() {
            return Err(RuntimeInstallError::InvalidServerLaunchOutput {
                path: argument_file.to_path_buf(),
                reason: format!(
                    "referenced loader library '{}' is missing",
                    referenced.display()
                ),
            });
        }
    }
    Ok(())
}

fn library_references(content: &str) -> Vec<String> {
    let normalized = content.replace('\\', "/");
    let mut references = Vec::new();
    let mut remaining = normalized.as_str();
    while let Some(start) = remaining.find("libraries/") {
        let candidate = &remaining[start..];
        let end = candidate
            .find(|character: char| {
                character.is_whitespace() || matches!(character, ';' | ':' | '\'' | '"' | ',')
            })
            .unwrap_or(candidate.len());
        let reference = &candidate[..end];
        if reference.ends_with(".jar") {
            references.push(reference.to_string());
        }
        remaining = &candidate[end..];
        if end == 0 {
            remaining = &candidate[1..];
        }
    }
    references.sort();
    references.dedup();
    references
}

struct RuntimeInstallLock {
    path: PathBuf,
    file: fs::File,
}

impl RuntimeInstallLock {
    fn acquire(path: &Path, timeout: Duration) -> Result<Self, RuntimeInstallError> {
        let parent = path
            .parent()
            .ok_or_else(|| RuntimeInstallError::InvalidProfile {
                reason: format!(
                    "runtime installation lock '{}' has no parent",
                    path.display()
                ),
            })?;
        fs::create_dir_all(parent).map_err(|source| RuntimeInstallError::Io {
            path: parent.display().to_string(),
            source,
        })?;
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)
            .map_err(|source| RuntimeInstallError::Io {
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
                        .map_err(|source| RuntimeInstallError::Io {
                            path: path.display().to_string(),
                            source,
                        })?;
                    return Ok(Self {
                        path: path.to_path_buf(),
                        file,
                    });
                }
                Err(std::fs::TryLockError::WouldBlock) if started.elapsed() < timeout => {
                    thread::sleep(Duration::from_millis(100));
                }
                Err(std::fs::TryLockError::WouldBlock) => {
                    return Err(RuntimeInstallError::CacheLockTimeout {
                        path: path.to_path_buf(),
                    });
                }
                Err(std::fs::TryLockError::Error(source)) => {
                    return Err(RuntimeInstallError::Io {
                        path: path.display().to_string(),
                        source,
                    });
                }
            }
        }
    }
}

impl Drop for RuntimeInstallLock {
    fn drop(&mut self) {
        if let Err(error) = self.file.unlock() {
            eprintln!(
                "failed to release runtime installation lock '{}': {error}",
                self.path.display()
            );
        }
    }
}

/// One library the loader installer would download, taken from its embedded `version.json`.
#[derive(Debug, Clone, PartialEq, Eq)]
struct InstallerLibrary {
    /// Repository path below `libraries/`, exactly as the installer lays it out.
    path: String,
    url: String,
    sha1: String,
    size: Option<u64>,
}

/// Copies the libraries an installer carries inside its own jar into the run directory.
///
/// Some installer entries have no download URL at all because the jar itself is the only place
/// they exist — Cleanroom ships its loader jar this way. The installer unpacks them during an
/// install, so reusing an installation without running the installer has to restore them too.
fn restore_installer_bundled_libraries(
    installer: &Path,
    run_dir: &Path,
    loader: &LoaderKind,
    progress: &mut dyn FnMut(&RuntimeInstallProgress),
) -> Result<(), RuntimeInstallError> {
    // A jar this code cannot read is not a reason to fail the installation: distribution
    // installers are plain archives, and anything else is left to the installer itself.
    let Ok(bundled) = read_installer_bundled_libraries(installer) else {
        return Ok(());
    };
    if bundled.is_empty() {
        return Ok(());
    }
    for library in bundled {
        let target = run_dir.join("libraries").join(&library.path);
        if valid_sha1_file(&target, &library.sha1)? {
            continue;
        }
        let bytes = library.bytes;
        let actual = format!("{:x}", Sha1::digest(&bytes));
        if actual != library.sha1 {
            return Err(RuntimeInstallError::InstallerLibraryChecksum {
                path: library.path,
                expected: library.sha1,
                actual,
            });
        }
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent).map_err(|source| RuntimeInstallError::Io {
                path: parent.display().to_string(),
                source,
            })?;
        }
        atomic_write_file(&target, &bytes)?;
        emit_cache_hit(
            progress,
            &format!("restored bundled {loader:?} library"),
            &target,
        );
    }
    Ok(())
}

/// Reads one installer's libraries and, when it declares one, its client jar.
///
/// Both come from the same embedded `version.json`, so one archive read serves the whole preseed.
fn read_installer_preseed_items(
    installer: &Path,
    minecraft_version: &str,
) -> Result<(Vec<InstallerLibrary>, Option<InstallerClientJar>), RuntimeInstallError> {
    let _ = minecraft_version;
    let file = fs::File::open(installer).map_err(|source| RuntimeInstallError::Io {
        path: installer.display().to_string(),
        source,
    })?;
    let mut archive = zip::ZipArchive::new(file).map_err(|source| {
        RuntimeInstallError::InvalidInstallerArchive {
            path: installer.to_path_buf(),
            reason: source.to_string(),
        }
    })?;
    let mut text = String::new();
    archive
        .by_name("version.json")
        .map_err(|source| RuntimeInstallError::InvalidInstallerArchive {
            path: installer.to_path_buf(),
            reason: format!("missing version.json: {source}"),
        })?
        .read_to_string(&mut text)
        .map_err(|source| RuntimeInstallError::InvalidInstallerArchive {
            path: installer.to_path_buf(),
            reason: format!("unreadable version.json: {source}"),
        })?;
    let profile = serde_json::from_str::<InstallerVersionJson>(&text)?;
    let client = read_installer_client_jar(installer, &profile);
    Ok((profile.into_libraries(), client))
}

/// Verifies that an embedded profile still describes the requested Cleanroom build.
///
/// The profile must name exactly one `com.cleanroommc:cleanroom` library at the requested version.
/// This is what makes publishing the embedded document equivalent to running the installer: the
/// installer rejects an installer jar whose bundled Cleanroom is not the version being installed.
fn validate_cleanroom_profile_libraries(
    bytes: &[u8],
    loader_version: &str,
) -> Result<(), RuntimeInstallError> {
    #[derive(Deserialize)]
    struct ProfileLibraries {
        #[serde(default)]
        libraries: Vec<ProfileLibrary>,
    }
    #[derive(Deserialize)]
    struct ProfileLibrary {
        #[serde(default)]
        name: String,
    }
    let profile: ProfileLibraries =
        serde_json::from_slice(bytes).map_err(RuntimeInstallError::Json)?;
    let mut cleanroom_versions = profile
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
    cleanroom_versions.sort();
    cleanroom_versions.dedup();
    if cleanroom_versions.len() != 1 || cleanroom_versions[0] != loader_version {
        return Err(RuntimeInstallError::InvalidProfile {
            reason: format!(
                "embedded Cleanroom profile lists libraries {cleanroom_versions:?} instead of exactly '{loader_version}'"
            ),
        });
    }
    Ok(())
}

/// Reads one installer's embedded `version.json` verbatim.
///
/// Cleanroom's installer carries the finished launcher profile in this entry and writes exactly
/// these bytes out during an install, so keeping the document intact lets the project publish the
/// same profile without starting the installer at all.
fn read_installer_version_json(installer: &Path) -> Result<Vec<u8>, RuntimeInstallError> {
    let file = fs::File::open(installer).map_err(|source| RuntimeInstallError::Io {
        path: installer.display().to_string(),
        source,
    })?;
    let mut archive = zip::ZipArchive::new(file).map_err(|source| {
        RuntimeInstallError::InvalidInstallerArchive {
            path: installer.to_path_buf(),
            reason: source.to_string(),
        }
    })?;
    let mut bytes = Vec::new();
    archive
        .by_name("version.json")
        .map_err(|source| RuntimeInstallError::InvalidInstallerArchive {
            path: installer.to_path_buf(),
            reason: format!("missing version.json: {source}"),
        })?
        .read_to_end(&mut bytes)
        .map_err(|source| RuntimeInstallError::InvalidInstallerArchive {
            path: installer.to_path_buf(),
            reason: format!("unreadable version.json: {source}"),
        })?;
    Ok(bytes)
}

/// Mirrors a repository URL onto the fastest known equivalent, in probe order.
///
/// The first entry is the original URL, so a host with no mirror still has exactly one
/// candidate. The transfer layer probes candidates and keeps whichever answers first.
fn artifact_candidate_urls(url: &str) -> Vec<String> {
    let mut candidates = vec![url.to_string()];
    for (from, to) in REPOSITORY_MIRRORS {
        if let Some(rest) = url.strip_prefix(from) {
            let mirrored = format!("{to}{rest}");
            if !candidates.contains(&mirrored) {
                candidates.push(mirrored);
            }
        }
    }
    candidates
}

/// Repository prefixes and the mirror that serves the same artifacts, in the order they are
/// mapped. Only prefixes whose mirror is a drop-in replacement for the same path belong here.
const REPOSITORY_MIRRORS: &[(&str, &str)] = &[
    (
        "https://repo.maven.apache.org/maven2",
        "https://maven.aliyun.com/repository/public",
    ),
    (
        "https://repo.cleanroommc.com/releases",
        "https://nexus.jsdu.cn/repository",
    ),
    (
        "https://maven.arcseekers.com/releases",
        "https://nexus.jsdu.cn/repository",
    ),
];

/// The Minecraft client jar the installer would fetch into `versions/<mc>/<mc>.jar`.
///
/// The installer checks that file first and only downloads it when it is absent, so fetching it
/// through the project downloader keeps a slow or failing Mojang connection from aborting the
/// whole installation.
#[derive(Debug, Clone, PartialEq, Eq)]
struct InstallerClientJar {
    url: String,
    sha1: String,
    size: Option<u64>,
}

/// Reads the client jar an installer's `version.json` names, when it names one.
fn read_installer_client_jar(
    installer: &Path,
    profile: &InstallerVersionJson,
) -> Option<InstallerClientJar> {
    let _ = installer;
    let artifact = profile.downloads.client.as_ref()?;
    let url = artifact.url.clone().filter(|url| !url.trim().is_empty())?;
    let sha1 = artifact
        .sha1
        .clone()
        .filter(|sha1| !sha1.trim().is_empty())?;
    Some(InstallerClientJar {
        url,
        sha1: sha1.to_ascii_lowercase(),
        size: artifact.size,
    })
}

/// One library the installer stores inside itself, with the exact bytes it holds.
struct InstallerBundledLibrary {
    path: String,
    sha1: String,
    bytes: Vec<u8>,
}

/// Reads the `maven/` payload of a loader installer.
///
/// These entries are already in repository layout below `maven/`, so the remainder of the path
/// is the same one `libraries/` uses.
fn read_installer_bundled_libraries(
    installer: &Path,
) -> Result<Vec<InstallerBundledLibrary>, RuntimeInstallError> {
    let file = fs::File::open(installer).map_err(|source| RuntimeInstallError::Io {
        path: installer.display().to_string(),
        source,
    })?;
    let mut archive = zip::ZipArchive::new(file).map_err(|source| {
        RuntimeInstallError::InvalidInstallerArchive {
            path: installer.to_path_buf(),
            reason: source.to_string(),
        }
    })?;
    let names = archive
        .file_names()
        .filter(|name| name.starts_with("maven/") && !name.ends_with('/'))
        .map(str::to_string)
        .collect::<Vec<_>>();
    let mut bundled = Vec::new();
    for name in names {
        let Some(path) = normalize_installer_library_path(&name["maven/".len()..]) else {
            continue;
        };
        let mut bytes = Vec::new();
        archive
            .by_name(&name)
            .map_err(|source| RuntimeInstallError::InvalidInstallerArchive {
                path: installer.to_path_buf(),
                reason: format!("unreadable bundled library '{name}': {source}"),
            })?
            .read_to_end(&mut bytes)
            .map_err(|source| RuntimeInstallError::InvalidInstallerArchive {
                path: installer.to_path_buf(),
                reason: format!("unreadable bundled library '{name}': {source}"),
            })?;
        let sha1 = format!("{:x}", Sha1::digest(&bytes));
        bundled.push(InstallerBundledLibrary { path, sha1, bytes });
    }
    Ok(bundled)
}

/// The parts of an installer's `version.json` this module needs.
#[derive(Debug, Deserialize)]
struct InstallerVersionJson {
    #[serde(default)]
    libraries: Vec<InstallerVersionLibrary>,
    #[serde(default)]
    downloads: InstallerVersionDownloadsRoot,
}

#[derive(Debug, Default, Deserialize)]
struct InstallerVersionDownloadsRoot {
    client: Option<InstallerVersionArtifact>,
}

#[derive(Debug, Deserialize)]
struct InstallerVersionLibrary {
    #[serde(default)]
    name: String,
    #[serde(default)]
    downloads: InstallerVersionDownloads,
}

#[derive(Debug, Default, Deserialize)]
struct InstallerVersionDownloads {
    artifact: Option<InstallerVersionArtifact>,
}

#[derive(Debug, Deserialize)]
struct InstallerVersionArtifact {
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    url: Option<String>,
    #[serde(default)]
    sha1: Option<String>,
    #[serde(default)]
    size: Option<u64>,
}

impl InstallerVersionJson {
    /// Returns one entry per distinct target path.
    ///
    /// The profile lists classifier variants of the same artifact more than once, and entries
    /// without an explicit path derive theirs from the coordinate, so several entries can name
    /// the same file. Repeats are dropped here because a transfer batch rejects any repeated
    /// task id and the whole preseed would then fail.
    fn into_libraries(self) -> Vec<InstallerLibrary> {
        let mut seen = BTreeSet::new();
        self.libraries
            .into_iter()
            .filter_map(|library| {
                let artifact = library.downloads.artifact?;
                let url = artifact.url.filter(|url| !url.trim().is_empty())?;
                let sha1 = artifact.sha1.filter(|sha1| !sha1.trim().is_empty())?;
                // Mojang-era entries carry no `path`; the installer derives it from the
                // coordinate, so the same coordinate has to drive the layout here.
                let raw_path = artifact
                    .path
                    .filter(|path| !path.trim().is_empty())
                    .or_else(|| maven_path_from_coordinate(&library.name))?;
                let path = normalize_installer_library_path(&raw_path)?;
                if !seen.insert(path.clone()) {
                    return None;
                }
                Some(InstallerLibrary {
                    path,
                    url,
                    sha1: sha1.to_ascii_lowercase(),
                    size: artifact.size,
                })
            })
            .collect()
    }
}

/// Derives the repository path below `libraries/` from a Maven coordinate.
///
/// `group:artifact:version` becomes `group/with/slashes/artifact/version/artifact-version.jar`,
/// matching how the installer lays out entries that omit an explicit path.
fn maven_path_from_coordinate(coordinate: &str) -> Option<String> {
    let (coordinate, extension) = coordinate
        .rsplit_once('@')
        .map_or((coordinate, "jar"), |(left, right)| (left, right));
    let parts = coordinate.split(':').collect::<Vec<_>>();
    if !(3..=4).contains(&parts.len())
        || parts.iter().any(|part| part.is_empty())
        || extension.is_empty()
    {
        return None;
    }
    let classifier = parts
        .get(3)
        .map(|classifier| format!("-{classifier}"))
        .unwrap_or_default();
    Some(format!(
        "{}/{}/{}/{}-{}{}.{}",
        parts[0].replace('.', "/"),
        parts[1],
        parts[2],
        parts[1],
        parts[2],
        classifier,
        extension
    ))
}

/// Keeps one installer library path below `libraries/`, rejecting anything that escapes it.
fn normalize_installer_library_path(path: &str) -> Option<String> {
    let normalized = path.replace('\\', "/");
    let relative = normalized.strip_prefix("libraries/").unwrap_or(&normalized);
    let usable = !relative.is_empty()
        && !Path::new(relative).is_absolute()
        && !relative.starts_with('/')
        && relative
            .split('/')
            .all(|component| !component.is_empty() && !matches!(component, "." | ".."));
    usable.then(|| relative.to_string())
}

fn verify_executable_server_jar(path: &Path) -> Result<(), RuntimeInstallError> {
    let file = fs::File::open(path).map_err(|source| RuntimeInstallError::Io {
        path: path.display().to_string(),
        source,
    })?;
    let mut archive = zip::ZipArchive::new(file).map_err(|source| {
        RuntimeInstallError::InvalidServerLaunchOutput {
            path: path.to_path_buf(),
            reason: source.to_string(),
        }
    })?;
    let mut manifest = archive.by_name("META-INF/MANIFEST.MF").map_err(|source| {
        RuntimeInstallError::InvalidServerLaunchOutput {
            path: path.to_path_buf(),
            reason: format!("missing manifest: {source}"),
        }
    })?;
    let mut text = String::new();
    manifest
        .read_to_string(&mut text)
        .map_err(|source| RuntimeInstallError::Io {
            path: path.display().to_string(),
            source,
        })?;
    if manifest_attribute(&text, "Main-Class").is_none_or(|value| value.is_empty()) {
        return Err(RuntimeInstallError::InvalidServerLaunchOutput {
            path: path.to_path_buf(),
            reason: "manifest does not declare a Main-Class".to_string(),
        });
    }
    Ok(())
}

fn relative_run_path(run_dir: &Path, output: &Path) -> Result<String, RuntimeInstallError> {
    let relative =
        output
            .strip_prefix(run_dir)
            .map_err(|_| RuntimeInstallError::InvalidProfile {
                reason: format!(
                    "server launch output '{}' is outside run directory '{}'",
                    output.display(),
                    run_dir.display()
                ),
            })?;
    Ok(relative.to_string_lossy().replace('\\', "/"))
}

fn safe_recorded_output_path(
    run_dir: &Path,
    relative: &str,
) -> Result<PathBuf, RuntimeInstallError> {
    let relative = Path::new(relative);
    if relative.as_os_str().is_empty()
        || relative.is_absolute()
        || relative.components().any(|component| {
            matches!(
                component,
                std::path::Component::ParentDir
                    | std::path::Component::RootDir
                    | std::path::Component::Prefix(_)
            )
        })
    {
        return Err(RuntimeInstallError::InvalidProfile {
            reason: "installation record contains an unsafe server output path".to_string(),
        });
    }
    Ok(run_dir.join(relative))
}

fn verify_cleanroom_server_jar(path: &Path) -> Result<(), RuntimeInstallError> {
    let file = fs::File::open(path).map_err(|source| RuntimeInstallError::Io {
        path: path.display().to_string(),
        source,
    })?;
    let mut archive = zip::ZipArchive::new(file).map_err(|source| {
        RuntimeInstallError::InvalidCleanroomServerJar {
            path: path.to_path_buf(),
            reason: source.to_string(),
        }
    })?;
    let mut manifest = archive.by_name("META-INF/MANIFEST.MF").map_err(|source| {
        RuntimeInstallError::InvalidCleanroomServerJar {
            path: path.to_path_buf(),
            reason: format!("missing manifest: {source}"),
        }
    })?;
    let mut text = String::new();
    manifest
        .read_to_string(&mut text)
        .map_err(|source| RuntimeInstallError::Io {
            path: path.display().to_string(),
            source,
        })?;
    let main_class = manifest_attribute(&text, "Main-Class");
    if main_class.as_deref().is_none_or(str::is_empty) {
        return Err(RuntimeInstallError::InvalidCleanroomServerJar {
            path: path.to_path_buf(),
            reason: "manifest does not declare a Main-Class".to_string(),
        });
    }
    Ok(())
}

fn find_cleanroom_server_jar(run_dir: &Path) -> Result<PathBuf, RuntimeInstallError> {
    let entries = fs::read_dir(run_dir).map_err(|source| RuntimeInstallError::Io {
        path: run_dir.display().to_string(),
        source,
    })?;
    let mut candidates = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|source| RuntimeInstallError::Io {
            path: run_dir.display().to_string(),
            source,
        })?;
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().to_ascii_lowercase();
        if !path.is_file()
            || !name.ends_with(".jar")
            || name.contains("installer")
            || !(name.starts_with("cleanroom-") || name == "server.jar")
        {
            continue;
        }
        if verify_cleanroom_server_jar(&path).is_ok() {
            candidates.push(path);
        }
    }
    candidates.sort();
    match candidates.len() {
        0 => Err(RuntimeInstallError::ServerLaunchOutputMissing {
            path: cleanroom_server_jar_path(run_dir, "<loader-version>"),
        }),
        1 => Ok(candidates.remove(0)),
        _ => Err(RuntimeInstallError::AmbiguousCleanroomServerJars { candidates }),
    }
}

fn manifest_attribute(manifest: &str, name: &str) -> Option<String> {
    let mut unfolded: Vec<String> = Vec::new();
    for line in manifest.lines() {
        if line.starts_with(' ') {
            if let Some(previous) = unfolded.last_mut() {
                previous.push_str(line.trim_start());
            }
        } else {
            unfolded.push(line.to_string());
        }
    }
    unfolded.into_iter().find_map(|line| {
        let (key, value) = line.split_once(':')?;
        key.eq_ignore_ascii_case(name)
            .then(|| value.trim().to_string())
    })
}

fn parse_sha1(value: &str, source: &str) -> Result<String, RuntimeInstallError> {
    if value.len() == 40 && value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Ok(value.to_ascii_lowercase());
    }
    Err(RuntimeInstallError::InstallerChecksumSidecar {
        url: source.to_string(),
        reason: "expected a 40-character SHA-1 digest".to_string(),
    })
}

fn parse_checksum_sidecar(checksum_url: &str, bytes: &[u8]) -> Result<String, RuntimeInstallError> {
    let checksum = std::str::from_utf8(bytes).map_err(|source| {
        RuntimeInstallError::InstallerChecksumSidecar {
            url: checksum_url.to_string(),
            reason: source.to_string(),
        }
    })?;
    let value = checksum.split_whitespace().next().ok_or_else(|| {
        RuntimeInstallError::InstallerChecksumSidecar {
            url: checksum_url.to_string(),
            reason: "checksum sidecar is empty".to_string(),
        }
    })?;
    parse_sha1(value, checksum_url)
}

fn verify_installer_sha1(
    installer_url: &str,
    expected_sha1: &str,
    bytes: &[u8],
) -> Result<(), RuntimeInstallError> {
    let actual_sha1 = format!("{:x}", Sha1::digest(bytes));
    if actual_sha1 == expected_sha1 {
        return Ok(());
    }
    Err(RuntimeInstallError::InstallerChecksumMismatch {
        url: installer_url.to_string(),
        expected: expected_sha1.to_string(),
        actual: actual_sha1,
    })
}

/// Creates a launcher source rooted in the standard run directory layout.
pub fn profile_source(run_dir: &Path) -> FileVersionJsonSource {
    FileVersionJsonSource::new(run_dir.join("versions"))
}

fn profile_path(run_dir: &Path, id: &str) -> PathBuf {
    run_dir.join("versions").join(id).join(format!("{id}.json"))
}

fn write_file(path: &Path, bytes: &[u8]) -> Result<(), RuntimeInstallError> {
    atomic_write_file(path, bytes)
}

fn atomic_write_file(path: &Path, bytes: &[u8]) -> Result<(), RuntimeInstallError> {
    let parent = path
        .parent()
        .ok_or_else(|| RuntimeInstallError::InvalidProfile {
            reason: format!("no parent directory for {}", path.display()),
        })?;
    fs::create_dir_all(parent).map_err(|source| RuntimeInstallError::Io {
        path: parent.display().to_string(),
        source,
    })?;
    let file_name = path
        .file_name()
        .ok_or_else(|| RuntimeInstallError::InvalidProfile {
            reason: format!("cache destination '{}' has no file name", path.display()),
        })?;
    let sequence = CACHE_TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let temporary = parent.join(format!(
        ".{}.{}.{}.tmp",
        file_name.to_string_lossy(),
        std::process::id(),
        sequence
    ));
    let result = (|| {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
            .map_err(|source| RuntimeInstallError::Io {
                path: temporary.display().to_string(),
                source,
            })?;
        file.write_all(bytes)
            .and_then(|_| file.flush())
            .and_then(|_| file.sync_all())
            .map_err(|source| RuntimeInstallError::Io {
                path: temporary.display().to_string(),
                source,
            })?;
        drop(file);
        replace_cache_file(&temporary, path).map_err(|source| RuntimeInstallError::Io {
            path: path.display().to_string(),
            source,
        })
    })();
    if result.is_err() && temporary.exists() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

#[cfg(not(windows))]
fn replace_cache_file(temporary: &Path, destination: &Path) -> std::io::Result<()> {
    fs::rename(temporary, destination)
}

#[cfg(windows)]
fn replace_cache_file(temporary: &Path, destination: &Path) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt;

    const MOVEFILE_REPLACE_EXISTING: u32 = 0x1;
    const MOVEFILE_WRITE_THROUGH: u32 = 0x8;
    #[link(name = "Kernel32")]
    unsafe extern "system" {
        fn MoveFileExW(existing: *const u16, replacement: *const u16, flags: u32) -> i32;
    }
    let existing = temporary
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect::<Vec<_>>();
    let replacement = destination
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect::<Vec<_>>();
    let moved = unsafe {
        MoveFileExW(
            existing.as_ptr(),
            replacement.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if moved == 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

fn copy_file_atomically(source: &Path, target: &Path) -> Result<(), RuntimeInstallError> {
    let bytes = fs::read(source).map_err(|source_error| RuntimeInstallError::Io {
        path: source.display().to_string(),
        source: source_error,
    })?;
    atomic_write_file(target, &bytes)
}

fn valid_sha1_file(path: &Path, expected_sha1: &str) -> Result<bool, RuntimeInstallError> {
    let mut file = match fs::File::open(path) {
        Ok(file) => file,
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(source) => {
            return Err(RuntimeInstallError::Io {
                path: path.display().to_string(),
                source,
            });
        }
    };
    let mut digest = Sha1::new();
    let mut buffer = [0_u8; 256 * 1024];
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|source| RuntimeInstallError::Io {
                path: path.display().to_string(),
                source,
            })?;
        if read == 0 {
            break;
        }
        digest.update(&buffer[..read]);
    }
    Ok(format!("{:x}", digest.finalize()).eq_ignore_ascii_case(expected_sha1))
}

fn remove_invalid_cache_file(path: &Path) -> Result<(), RuntimeInstallError> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(RuntimeInstallError::Io {
            path: path.display().to_string(),
            source,
        }),
    }
}

fn emit_cache_hit(
    progress: &mut dyn FnMut(&RuntimeInstallProgress),
    description: &str,
    path: &Path,
) {
    progress(&RuntimeInstallProgress::InstallerOutput {
        stream: RuntimeInstallStream::Stdout,
        line: format!("[cache] reused {description}: {}", path.display()),
    });
}

fn safe_cache_component(value: &str) -> String {
    value
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '.' | '-' | '_') {
                character
            } else {
                '_'
            }
        })
        .collect()
}

fn absolute_installer_path(installer: &Path) -> Result<PathBuf, RuntimeInstallError> {
    fs::metadata(installer).map_err(|source| RuntimeInstallError::InstallerPath {
        path: installer.to_path_buf(),
        source,
    })?;
    ordinary_absolute_path(installer).map_err(|source| RuntimeInstallError::InstallerPath {
        path: installer.to_path_buf(),
        source,
    })
}

fn absolute_installer_target(run_dir: &Path) -> Result<PathBuf, RuntimeInstallError> {
    ordinary_absolute_path(run_dir).map_err(|source| RuntimeInstallError::InstallerTargetPath {
        path: run_dir.to_path_buf(),
        source,
    })
}

fn ordinary_absolute_path(path: &Path) -> std::io::Result<PathBuf> {
    Ok(without_windows_verbatim_prefix(std::path::absolute(path)?))
}

#[cfg(not(windows))]
fn without_windows_verbatim_prefix(path: PathBuf) -> PathBuf {
    path
}

#[cfg(windows)]
fn without_windows_verbatim_prefix(path: PathBuf) -> PathBuf {
    use std::os::windows::ffi::{OsStrExt, OsStringExt};

    let units: Vec<u16> = path.as_os_str().encode_wide().collect();
    let verbatim_unc: Vec<u16> = r"\\?\UNC\".encode_utf16().collect();
    if let Some(rest) = units.strip_prefix(verbatim_unc.as_slice()) {
        let mut ordinary = vec![b'\\' as u16, b'\\' as u16];
        ordinary.extend_from_slice(rest);
        return PathBuf::from(OsString::from_wide(&ordinary));
    }
    let verbatim: Vec<u16> = r"\\?\".encode_utf16().collect();
    if let Some(rest) = units.strip_prefix(verbatim.as_slice()) {
        return PathBuf::from(OsString::from_wide(rest));
    }
    path
}

fn client_installer_arguments(
    loader: &LoaderKind,
    installer: &Path,
    target: &Path,
) -> Result<[OsString; 4], RuntimeInstallError> {
    if *loader == LoaderKind::Fabric {
        return Err(RuntimeInstallError::UnsupportedJarClientInstaller {
            loader: loader.clone(),
        });
    }
    Ok([
        OsString::from("-jar"),
        installer.as_os_str().to_owned(),
        OsString::from("--installClient"),
        target.as_os_str().to_owned(),
    ])
}

/// Converts an HTTP(S) proxy URL into JVM system properties for a loader installer process.
///
/// Loader installers create their own HTTP clients, so the proxy used by mcmpu's Rust HTTP
/// client is not inherited automatically. This function deliberately returns only JVM
/// arguments: callers must not include them in command previews or progress logs because a
/// proxy URL may contain credentials.
pub fn installer_jvm_proxy_arguments(
    proxy: Option<&str>,
) -> Result<Vec<String>, RuntimeInstallError> {
    let Some(proxy) = proxy.map(str::trim).filter(|proxy| !proxy.is_empty()) else {
        return Ok(Vec::new());
    };
    let parsed =
        reqwest::Url::parse(proxy).map_err(|_| RuntimeInstallError::InvalidInstallerProxy {
            reason: "expected a complete http:// or https:// proxy URL",
        })?;
    let scheme = parsed.scheme();
    if scheme != "http" && scheme != "https" {
        return Err(RuntimeInstallError::InvalidInstallerProxy {
            reason: "only http:// and https:// proxy URLs are supported by loader installers",
        });
    }
    let host = parsed
        .host_str()
        .ok_or(RuntimeInstallError::InvalidInstallerProxy {
            reason: "proxy URL must include a host",
        })?;
    let port =
        parsed
            .port_or_known_default()
            .ok_or(RuntimeInstallError::InvalidInstallerProxy {
                reason: "proxy URL must include a port or use a scheme with a default port",
            })?;

    let mut arguments = Vec::with_capacity(8);
    for protocol in ["http", "https"] {
        arguments.push(format!("-D{protocol}.proxyHost={host}"));
        arguments.push(format!("-D{protocol}.proxyPort={port}"));
    }
    if !parsed.username().is_empty() {
        let username = decode_proxy_credential(parsed.username())?;
        for protocol in ["http", "https"] {
            arguments.push(format!("-D{protocol}.proxyUser={username}"));
        }
    }
    if let Some(password) = parsed.password() {
        let password = decode_proxy_credential(password)?;
        for protocol in ["http", "https"] {
            arguments.push(format!("-D{protocol}.proxyPassword={password}"));
        }
    }
    Ok(arguments)
}

fn decode_proxy_credential(value: &str) -> Result<String, RuntimeInstallError> {
    let value = percent_encoding::percent_decode_str(value)
        .decode_utf8()
        .map_err(|_| RuntimeInstallError::InvalidInstallerProxy {
            reason: "proxy credentials must be valid UTF-8 after URL decoding",
        })?
        .into_owned();
    if value.chars().any(char::is_control) {
        return Err(RuntimeInstallError::InvalidInstallerProxy {
            reason: "proxy credentials must not contain control characters",
        });
    }
    Ok(value)
}

fn installer_failure_details(status: &str, transcript: &[String]) -> String {
    let mut details = format!("process exited with status {status}");
    if !transcript.is_empty() {
        details.push('\n');
        details.push_str(&transcript.join("\n"));
    }
    details
}

fn write_client_launcher_profiles(run_dir: &Path) -> Result<(), RuntimeInstallError> {
    write_file(
        &run_dir.join("launcher_profiles.json"),
        CLIENT_LAUNCHER_PROFILES.as_bytes(),
    )
}

/// Identity of one loader installation, used to invalidate loader-owned run directories.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct LibrariesMarker {
    version: u32,
    loader: LoaderKind,
    minecraft: String,
    loader_version: String,
}

impl LibrariesMarker {
    fn for_config(config: &Config) -> Self {
        Self {
            version: LIBRARIES_MARKER_VERSION,
            loader: config.pack.loader.clone(),
            minecraft: config.pack.minecraft_version.clone(),
            loader_version: config.pack.loader_version.clone(),
        }
    }
}

/// Clears loader-owned run directories when the installed loader no longer matches the project.
///
/// `libraries` and `versions` hold files produced by one specific loader installation. Reusing
/// them across a loader or loader-version switch leaves the previous loader's jars on the
/// classpath, which breaks launch with mismatched or duplicate classes. The marker keeps the
/// fast path for an unchanged loader while forcing a clean reinstall after any switch.
fn reset_loader_directories(
    run_dir: &Path,
    config: &Config,
    progress: &mut dyn FnMut(&RuntimeInstallProgress),
) -> Result<(), RuntimeInstallError> {
    let marker_path = run_dir.join(LIBRARIES_MARKER_NAME);
    let expected = LibrariesMarker::for_config(config);
    let recorded = match fs::read(&marker_path) {
        Ok(bytes) => serde_json::from_slice::<LibrariesMarker>(&bytes).ok(),
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => None,
        Err(source) => {
            return Err(RuntimeInstallError::Io {
                path: marker_path.display().to_string(),
                source,
            });
        }
    };
    if recorded.as_ref() == Some(&expected) {
        return Ok(());
    }
    // Switching loader family means the two installations share almost nothing, so those
    // directories are cleared outright.
    //
    // A Cleanroom version bump is different: consecutive Cleanroom builds carry nearly the same
    // library set, and every library is verified against the size and SHA-1 the current profile
    // demands before it is used, so a leftover jar from the other build is either overwritten or
    // never referenced. Clearing `libraries/` there only discarded the ~140 shared jars and made
    // the next start download them again.
    let keep_libraries = match recorded.as_ref() {
        Some(previous) => {
            previous.loader == config.pack.loader && config.pack.loader == LoaderKind::Cleanroom
        }
        None => false,
    };
    let mut directories = vec!["versions"];
    if !keep_libraries {
        directories.push("libraries");
    }
    for name in directories {
        let path = run_dir.join(name);
        match fs::remove_dir_all(&path) {
            Ok(()) => {}
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => {}
            Err(source) => {
                return Err(RuntimeInstallError::Io {
                    path: path.display().to_string(),
                    source,
                });
            }
        }
    }
    let description = match recorded {
        Some(previous) => format!(
            "loader installation for {:?} {} (was {:?} {})",
            config.pack.loader,
            config.pack.loader_version,
            previous.loader,
            previous.loader_version
        ),
        None => format!(
            "loader installation for {:?} {} (no previous marker)",
            config.pack.loader, config.pack.loader_version
        ),
    };
    emit_cache_hit(progress, &format!("cleared {description}"), &marker_path);
    Ok(())
}

/// Fetches the libraries an installer would download on its own, before the installer runs.
///
/// The installer verifies each file it needs against its embedded SHA-1 and skips the ones that
/// already match, so filling the directories with verified files first removes the bulk of its
/// single-threaded downloading. Files that are already valid are left untouched.
///
/// Failures are reported to the caller but never abort the installation: the installer still
/// fetches whatever is missing itself.
/// Reports that the parallel preseed could not run, so the user knows why the installer is
/// downloading everything by itself instead of the loader libraries having arrived first.
fn warn_preseed_skipped(
    progress: &mut dyn FnMut(&RuntimeInstallProgress),
    installer: &Path,
    error: &RuntimeInstallError,
) {
    emit_cache_hit(
        progress,
        &format!(
            "warning: could not pre-download loader libraries ({error}); the installer will fetch them itself, which is slower"
        ),
        installer,
    );
}

fn preseed_installer_libraries(
    downloader: &dyn RuntimeInstallDownloader,
    run_dir: &Path,
    installer: &Path,
    minecraft_version: &str,
    progress: &mut dyn FnMut(&RuntimeInstallProgress),
) -> Result<(), RuntimeInstallError> {
    let (libraries, client_jar) = read_installer_preseed_items(installer, minecraft_version)?;
    if libraries.is_empty() && client_jar.is_none() {
        return Ok(());
    }
    // The installer skips its own client jar download when the file already exists, so it is
    // prepared here and counted with the libraries: leaving it out of the total made progress
    // report one more item than it claimed to have.
    let client_request = match client_jar {
        Some(client) => {
            let target = run_dir
                .join("versions")
                .join(minecraft_version)
                .join(format!("{minecraft_version}.jar"));
            if valid_sha1_file(&target, &client.sha1)? {
                None
            } else {
                if let Some(parent) = target.parent() {
                    fs::create_dir_all(parent).map_err(|source| RuntimeInstallError::Io {
                        path: parent.display().to_string(),
                        source,
                    })?;
                }
                Some(InstallerLibraryRequest {
                    url: client.url,
                    target,
                    sha1: client.sha1,
                })
            }
        }
        None => None,
    };
    let total = libraries.len() + usize::from(client_request.is_some());
    progress(&RuntimeInstallProgress::InstallingLibraries { total });
    let mut pending = Vec::new();
    let mut completed = 0usize;
    for library in libraries {
        let target = run_dir.join("libraries").join(&library.path);
        if valid_sha1_file(&target, &library.sha1)? {
            completed += 1;
            progress(&RuntimeInstallProgress::LibraryReady {
                completed,
                total,
                path: library.path,
            });
            continue;
        }
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent).map_err(|source| RuntimeInstallError::Io {
                path: parent.display().to_string(),
                source,
            })?;
        }
        pending.push(InstallerLibraryRequest {
            url: library.url,
            target,
            sha1: library.sha1,
        });
    }
    // The installer skips its own client jar download when the file already exists.
    if let Some(client) = client_request {
        pending.push(client);
    }
    if pending.is_empty() {
        return Ok(());
    }
    // Libraries are fetched in chunks rather than one batch: a batch returns only after every
    // request in it has settled, so a single batch left the console silent for the whole
    // download and then printed everything at once. Failures are collected the same way either
    // approach, and the libraries that did arrive are kept so the installer still has less work.
    let mut failed = 0usize;
    let mut first_error = None;
    for chunk in pending.chunks(INSTALLER_LIBRARY_BATCH_SIZE) {
        let results = downloader.download_artifacts(chunk);
        if results.len() != chunk.len() {
            return Err(RuntimeInstallError::InstallerLibraryBatch {
                reason: format!(
                    "downloader returned {} result(s) for {} request(s)",
                    results.len(),
                    chunk.len()
                ),
            });
        }
        for (request, result) in chunk.iter().zip(results) {
            match result {
                Ok(()) if valid_sha1_file(&request.target, &request.sha1)? => {
                    completed += 1;
                    progress(&RuntimeInstallProgress::LibraryReady {
                        completed,
                        total,
                        path: relative_run_path(run_dir, &request.target)?,
                    });
                }
                Ok(()) => {
                    failed += 1;
                    first_error.get_or_insert_with(|| RuntimeInstallError::InstallerLibraryBatch {
                        reason: format!(
                            "downloaded library '{}' failed checksum verification",
                            request.target.display()
                        ),
                    });
                }
                Err(error) => {
                    failed += 1;
                    first_error.get_or_insert(error);
                }
            }
        }
    }
    if failed == 0 {
        return Ok(());
    }
    // Report the count as well as the cause: a few failures are worth retrying by hand, while a
    // rejected batch means nothing was fetched and the installer does all of the work.
    let detail = first_error
        .map(|error| error.to_string())
        .unwrap_or_else(|| "unknown cause".to_string());
    Err(RuntimeInstallError::InstallerLibraryBatch {
        reason: format!("{failed} of {total} libraries failed; first failure: {detail}"),
    })
}

/// Records the loader installation that now owns the run directories.
fn write_libraries_marker(run_dir: &Path, config: &Config) -> Result<(), RuntimeInstallError> {
    let bytes = serde_json::to_vec_pretty(&LibrariesMarker::for_config(config))?;
    atomic_write_file(&run_dir.join(LIBRARIES_MARKER_NAME), &bytes)
}

pub fn find_loader_profile(
    run_dir: &Path,
    loader: &LoaderKind,
    minecraft: &str,
    loader_version: &str,
) -> Result<String, RuntimeInstallError> {
    let versions = run_dir.join("versions");
    let entries = fs::read_dir(&versions).map_err(|source| RuntimeInstallError::Io {
        path: versions.display().to_string(),
        source,
    })?;
    let mut candidates = Vec::new();
    let mut rejected_candidates = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|source| RuntimeInstallError::Io {
            path: versions.display().to_string(),
            source,
        })?;
        let directory_id =
            entry
                .file_name()
                .into_string()
                .map_err(|_| RuntimeInstallError::InvalidProfile {
                    reason: format!(
                        "installer produced non-Unicode version directory under {}",
                        versions.display()
                    ),
                })?;
        if !directory_id.contains(loader_version) {
            continue;
        }
        let path = profile_path(run_dir, &directory_id);
        let text = fs::read_to_string(&path).map_err(|source| RuntimeInstallError::Io {
            path: path.display().to_string(),
            source,
        })?;
        let profile: InstalledProfile = serde_json::from_str(&text)?;
        if profile.id != directory_id {
            return Err(RuntimeInstallError::InvalidProfile {
                reason: format!(
                    "installer profile '{}' declares id '{}'",
                    path.display(),
                    profile.id
                ),
            });
        }
        match validate_loader_profile(
            profile,
            loader,
            minecraft,
            loader_version,
            Some(&directory_id),
        ) {
            Ok(profile) => candidates.push(profile.id),
            Err(RuntimeInstallError::InvalidProfile { reason }) => rejected_candidates.push(reason),
            Err(error) => return Err(error),
        }
    }
    candidates.sort();
    match candidates.len() {
        0 if rejected_candidates.is_empty() => {
            Err(RuntimeInstallError::ProfileNotProduced { versions })
        }
        0 => Err(RuntimeInstallError::InvalidProfile {
            reason: format!(
                "installer produced profile candidates for loader version '{loader_version}', but none matched {loader:?} on Minecraft '{minecraft}': {}",
                rejected_candidates.join("; ")
            ),
        }),
        1 => Ok(candidates.remove(0)),
        _ => Err(RuntimeInstallError::AmbiguousProfile {
            versions,
            candidates,
        }),
    }
}

fn valid_profile_id_file(path: &Path, expected_id: &str) -> Result<bool, RuntimeInstallError> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(source) => {
            return Err(RuntimeInstallError::Io {
                path: path.display().to_string(),
                source,
            });
        }
    };
    Ok(validate_profile_id_bytes(&bytes, expected_id).is_ok())
}

fn validate_profile_id_bytes(bytes: &[u8], expected_id: &str) -> Result<(), RuntimeInstallError> {
    let profile: ProfileId = serde_json::from_slice(bytes)?;
    if profile.id != expected_id {
        return Err(RuntimeInstallError::InvalidProfile {
            reason: format!(
                "version profile declares id '{}' instead of '{expected_id}'",
                profile.id
            ),
        });
    }
    Ok(())
}

fn validate_client_profile(
    run_dir: &Path,
    loader: &LoaderKind,
    profile_id: &str,
    minecraft: &str,
    loader_version: &str,
) -> Result<(), RuntimeInstallError> {
    let path = profile_path(run_dir, profile_id);
    let bytes = fs::read(&path).map_err(|source| RuntimeInstallError::Io {
        path: path.display().to_string(),
        source,
    })?;
    validate_loader_profile_bytes(&bytes, loader, minecraft, loader_version, Some(profile_id))?;
    LoaderLaunchResolver::new(profile_source(run_dir))
        .resolve_client(
            loader.clone(),
            profile_id,
            &validation_launch_environment()?,
        )
        .map_err(|source| RuntimeInstallError::InvalidProfile {
            reason: format!("loader launch profile '{profile_id}' cannot be resolved: {source}"),
        })?;
    Ok(())
}

fn validation_launch_environment() -> Result<LaunchEnvironment, RuntimeInstallError> {
    let os_name = match std::env::consts::OS {
        "windows" => "windows",
        "linux" => "linux",
        "macos" => "osx",
        other => {
            return Err(RuntimeInstallError::InvalidProfile {
                reason: format!("runtime profile validation is unsupported on '{other}'"),
            });
        }
    };
    Ok(LaunchEnvironment::new(
        os_name,
        std::env::consts::ARCH,
        if cfg!(target_pointer_width = "64") {
            64
        } else {
            32
        },
    ))
}

fn read_valid_loader_profile(
    path: &Path,
    loader: &LoaderKind,
    minecraft: &str,
    loader_version: &str,
    expected_id: Option<&str>,
) -> Result<Option<InstalledProfile>, RuntimeInstallError> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(source) => {
            return Err(RuntimeInstallError::Io {
                path: path.display().to_string(),
                source,
            });
        }
    };
    Ok(validate_loader_profile_bytes(&bytes, loader, minecraft, loader_version, expected_id).ok())
}

fn validate_loader_profile_bytes(
    bytes: &[u8],
    loader: &LoaderKind,
    minecraft: &str,
    loader_version: &str,
    expected_id: Option<&str>,
) -> Result<InstalledProfile, RuntimeInstallError> {
    let profile: InstalledProfile = serde_json::from_slice(bytes)?;
    validate_loader_profile(profile, loader, minecraft, loader_version, expected_id)
}

fn validate_loader_profile(
    profile: InstalledProfile,
    loader: &LoaderKind,
    minecraft: &str,
    loader_version: &str,
    expected_id: Option<&str>,
) -> Result<InstalledProfile, RuntimeInstallError> {
    if profile.id.trim().is_empty() {
        return Err(RuntimeInstallError::InvalidProfile {
            reason: "loader profile id is empty".to_string(),
        });
    }
    if expected_id.is_some_and(|expected| profile.id != expected) {
        return Err(RuntimeInstallError::InvalidProfile {
            reason: format!("loader profile declares unexpected id '{}'", profile.id),
        });
    }
    match profile.inherits_from.as_deref() {
        Some(parent) if parent == minecraft => {}
        Some(parent) => {
            return Err(RuntimeInstallError::InvalidProfile {
                reason: format!(
                    "loader profile '{}' inherits from Minecraft '{parent}' instead of '{minecraft}'",
                    profile.id
                ),
            });
        }
        None if *loader == LoaderKind::Cleanroom => {
            validate_cleanroom_standalone_profile(&profile, loader_version)?;
        }
        None => {
            return Err(RuntimeInstallError::InvalidProfile {
                reason: format!(
                    "{loader:?} loader profile '{}' does not inherit from Minecraft '{minecraft}'",
                    profile.id
                ),
            });
        }
    }
    Ok(profile)
}

fn validate_cleanroom_standalone_profile(
    profile: &InstalledProfile,
    loader_version: &str,
) -> Result<(), RuntimeInstallError> {
    let id = profile.id.as_str();
    // Cleanroom names its profile differently across releases: older builds embed the Minecraft
    // version ("1.12.2-Cleanroom-0.6.13-alpha") while newer ones carry only the loader
    // ("Cleanroom-0.7.2-alpha"). The loader version is the part the launch depends on; the
    // Minecraft version is pinned by the vanilla profile the run directory already holds, so it
    // must not be demanded of the name here.
    if !id_contains_component(id, "cleanroom", true)
        || !id_contains_component(id, loader_version, false)
    {
        return Err(RuntimeInstallError::InvalidProfile {
            reason: format!(
                "standalone Cleanroom profile '{id}' must identify Cleanroom and loader version '{loader_version}'"
            ),
        });
    }
    if profile
        .main_class
        .as_deref()
        .is_none_or(|main_class| main_class.trim().is_empty())
    {
        return Err(RuntimeInstallError::InvalidProfile {
            reason: format!(
                "standalone Cleanroom profile '{}' does not declare a non-empty mainClass",
                profile.id
            ),
        });
    }
    Ok(())
}

fn id_contains_component(id: &str, component: &str, ignore_ascii_case: bool) -> bool {
    if component.is_empty() {
        return false;
    }
    let haystack = if ignore_ascii_case {
        id.to_ascii_lowercase()
    } else {
        id.to_string()
    };
    let needle = if ignore_ascii_case {
        component.to_ascii_lowercase()
    } else {
        component.to_string()
    };
    haystack.match_indices(&needle).any(|(start, _)| {
        let end = start + needle.len();
        let before_is_boundary = haystack[..start]
            .chars()
            .next_back()
            .is_none_or(|character| !character.is_ascii_alphanumeric());
        let after_is_boundary = haystack[end..]
            .chars()
            .next()
            .is_none_or(|character| !character.is_ascii_alphanumeric());
        before_is_boundary && after_is_boundary
    })
}

fn parse_version_manifest(bytes: &[u8]) -> Result<VersionManifest, RuntimeInstallError> {
    let manifest: VersionManifest = serde_json::from_slice(bytes)?;
    if manifest
        .versions
        .iter()
        .any(|entry| entry.id.trim().is_empty() || entry.url.trim().is_empty())
    {
        return Err(RuntimeInstallError::InvalidProfile {
            reason: "Mojang version manifest contains an empty version id or URL".to_string(),
        });
    }
    Ok(manifest)
}

fn read_valid_version_manifest(
    path: &Path,
) -> Result<Option<VersionManifest>, RuntimeInstallError> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(source) => {
            return Err(RuntimeInstallError::Io {
                path: path.display().to_string(),
                source,
            });
        }
    };
    Ok(parse_version_manifest(&bytes).ok())
}

#[derive(Debug, Deserialize)]
struct VersionManifest {
    versions: Vec<VersionManifestEntry>,
}
#[derive(Debug, Deserialize)]
struct VersionManifestEntry {
    id: String,
    url: String,
}
#[derive(Debug, Deserialize)]
struct ProfileId {
    id: String,
}
#[derive(Debug, Deserialize)]
struct InstalledProfile {
    id: String,
    #[serde(rename = "inheritsFrom")]
    inherits_from: Option<String>,
    #[serde(rename = "mainClass")]
    main_class: Option<String>,
}

/// Errors from downloading, installing, or locating a real loader profile.
#[derive(Debug, Error)]
pub enum RuntimeInstallError {
    #[error(transparent)]
    Network(#[from] NetworkError),
    #[error(transparent)]
    Transfer(#[from] TransferError),
    #[error("runtime metadata '{url}' exceeded the {limit}-byte limit")]
    MetadataTooLarge { url: String, limit: usize },
    #[error("failed to parse runtime profile metadata: {0}")]
    Json(#[from] serde_json::Error),
    #[error("Minecraft version '{version}' is absent from Mojang's version manifest")]
    MinecraftVersionMissing { version: String },
    #[error("failed to prepare '{path}': {source}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("invalid loader installer proxy: {reason}")]
    InvalidInstallerProxy { reason: &'static str },
    #[error("invalid loader installer archive '{path}': {reason}")]
    InvalidInstallerArchive { path: PathBuf, reason: String },
    #[error("failed to prepare installer library downloads: {reason}")]
    InstallerLibraryBatch { reason: String },
    #[error("bundled installer library '{path}' has SHA-1 {actual}, expected {expected}")]
    InstallerLibraryChecksum {
        path: String,
        expected: String,
        actual: String,
    },
    #[error("failed to configure the loader installer HTTP client for the proxy")]
    InstallerProxyClientConfiguration,
    #[error("failed to create loader installation plan: {source}")]
    LoaderPlan {
        #[source]
        source: crate::server::loader::LoaderPlanError,
    },
    #[error("failed to start loader installer '{path}': {source}")]
    InstallerStart {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("loader installer process '{path}' could not be completed: {source}")]
    InstallerProcess {
        path: PathBuf,
        #[source]
        source: InstallerProcessError,
    },
    #[error(
        "downloaded loader installer '{path}' cannot be resolved to an absolute path: {source}"
    )]
    InstallerPath {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error(
        "client loader installer target '{path}' cannot be resolved to an absolute path: {source}"
    )]
    InstallerTargetPath {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error(
        "server launch output '{output}' is not inside run directory '{run_dir}', so it cannot be          passed to the server JVM"
    )]
    ServerLaunchOutputOutsideRunDirectory { output: PathBuf, run_dir: PathBuf },
    #[error("loader '{loader:?}' does not use a JAR client installer")]
    UnsupportedJarClientInstaller { loader: LoaderKind },
    #[error("loader installer '{path}' failed: {details}")]
    InstallerFailed { path: PathBuf, details: String },
    #[error("loader installer checksum sidecar '{url}' is invalid: {reason}")]
    InstallerChecksumSidecar { url: String, reason: String },
    #[error("loader installer '{url}' SHA-1 mismatch: expected {expected}, got {actual}")]
    InstallerChecksumMismatch {
        url: String,
        expected: String,
        actual: String,
    },
    #[error("loader installer did not produce a version profile under '{versions}'")]
    ProfileNotProduced { versions: PathBuf },
    #[error(
        "loader installer produced ambiguous matching profiles under '{versions}': {candidates:?}"
    )]
    AmbiguousProfile {
        versions: PathBuf,
        candidates: Vec<String>,
    },
    #[error("invalid runtime profile: {reason}")]
    InvalidProfile { reason: String },
    #[error("server installer did not produce required launch output '{path}'")]
    ServerLaunchOutputMissing { path: PathBuf },
    #[error("server installer output '{path}' is invalid: {reason}")]
    InvalidServerLaunchOutput { path: PathBuf, reason: String },
    #[error("Cleanroom server launch jar '{path}' is invalid: {reason}")]
    InvalidCleanroomServerJar { path: PathBuf, reason: String },
    #[error("Cleanroom installer produced multiple server launch jars: {candidates:?}")]
    AmbiguousCleanroomServerJars { candidates: Vec<PathBuf> },
    #[error("timed out waiting for runtime installation cache lock '{path}'")]
    CacheLockTimeout { path: PathBuf },
}

#[cfg(test)]
mod tests {
    use std::io::Write;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    use super::{
        InstallerLibraryRequest, preseed_installer_libraries, read_installer_preseed_items,
        restore_installer_bundled_libraries,
    };

    use crate::config::{
        BuildConfig, Config, JavaConfig, LoaderKind, NetworkConfig, PackConfig, TestRunConfig,
    };
    use sha1::Digest;

    use super::{
        InstallerProcessResult, InstallerProcessRunner, InstallerProcessSpec,
        InstallerTranscriptTail, MAX_INSTALLER_TRANSCRIPT_BYTES, MAX_INSTALLER_TRANSCRIPT_LINES,
        RuntimeInstallDownloader, RuntimeInstallError, RuntimeInstallProgress,
        RuntimeInstallStream, RuntimeProfileLocator, RuntimeProfileLocatorImpl,
        SystemInstallerProcessRunner, VERSION_MANIFEST_URL, absolute_installer_path,
        absolute_installer_target, cleanroom_server_jar_path, client_installer_arguments,
        find_cleanroom_server_jar, installer_failure_details, server_launch_output_path,
        verify_cleanroom_server_jar, verify_installer_sha1, write_client_launcher_profiles,
    };

    #[test]
    fn cleanroom_server_jar_is_the_installer_output_and_requires_server_wrapper() {
        let temp = tempfile::tempdir().unwrap();
        let path = cleanroom_server_jar_path(temp.path(), "0.3.16-alpha");
        let file = std::fs::File::create(&path).unwrap();
        let mut archive = zip::ZipWriter::new(file);
        archive
            .start_file(
                "META-INF/MANIFEST.MF",
                zip::write::SimpleFileOptions::default(),
            )
            .unwrap();
        archive
            .write_all(b"Manifest-Version: 1.0\nMain-Class: net.minecraftforge.fml.relauncher.ServerLaunchWrapper\n")
            .unwrap();
        archive.finish().unwrap();

        verify_cleanroom_server_jar(&path).unwrap();
        assert_eq!(
            path.file_name().unwrap().to_string_lossy(),
            "cleanroom-0.3.16-alpha.jar"
        );
    }

    #[test]
    fn cleanroom_server_locator_uses_installer_output_instead_of_version_file_name() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("cleanroom-server-launcher.jar");
        write_cleanroom_server_jar(
            &path,
            b"Manifest-Version: 1.0\nMain-Class: net.minecraftforge.fml.relauncher.Server\n LaunchWrapper\n",
        );

        let selected = find_cleanroom_server_jar(temp.path()).unwrap();

        assert_eq!(selected, path);
    }

    #[test]
    fn cleanroom_server_locator_rejects_ambiguous_installation_output() {
        let temp = tempfile::tempdir().unwrap();
        write_cleanroom_server_jar(
            &temp.path().join("cleanroom-first.jar"),
            b"Manifest-Version: 1.0\nMain-Class: first.Main\n",
        );
        write_cleanroom_server_jar(
            &temp.path().join("cleanroom-second.jar"),
            b"Manifest-Version: 1.0\nMain-Class: second.Main\n",
        );

        let error = find_cleanroom_server_jar(temp.path()).unwrap_err();

        assert!(matches!(
            error,
            RuntimeInstallError::AmbiguousCleanroomServerJars { .. }
        ));
    }

    #[test]
    fn installer_checksum_mismatch_is_rejected_before_any_installer_can_run() {
        let error = verify_installer_sha1(
            "https://repo.example/loader-installer.jar",
            "0000000000000000000000000000000000000000",
            b"tampered installer",
        )
        .unwrap_err();

        assert!(matches!(
            error,
            RuntimeInstallError::InstallerChecksumMismatch { .. }
        ));
    }

    #[test]
    fn jar_client_preparation_writes_a_valid_launcher_profile() {
        let temp = tempfile::tempdir().unwrap();

        write_client_launcher_profiles(temp.path()).unwrap();

        let content = std::fs::read_to_string(temp.path().join("launcher_profiles.json")).unwrap();
        let profile: serde_json::Value = serde_json::from_str(&content).unwrap();
        assert_eq!(profile["selectedProfile"], "(Default)");
        assert_eq!(profile["profiles"], serde_json::json!({}));
        assert_eq!(profile["clientToken"], "00000000000000000000000000000000");
    }

    #[test]
    fn installer_path_is_absolute_before_java_is_spawned() {
        let temp = tempfile::tempdir().unwrap();
        let installer = temp.path().join(".mcmpu/loader-installer.jar");
        std::fs::create_dir_all(installer.parent().unwrap()).unwrap();
        std::fs::write(&installer, b"fixture installer").unwrap();

        let absolute = absolute_installer_path(&installer).unwrap();

        assert!(absolute.is_absolute());
        assert!(absolute.is_file());
        #[cfg(windows)]
        assert!(!absolute.to_string_lossy().starts_with(r"\\?\"));
    }

    #[test]
    fn client_installer_command_explicitly_targets_the_run_directory() {
        let temp = tempfile::tempdir().unwrap();
        let installer = temp.path().join(".mcmpu/loader-installer.jar");
        std::fs::create_dir_all(installer.parent().unwrap()).unwrap();
        std::fs::write(&installer, b"fixture installer").unwrap();

        let installer = absolute_installer_path(&installer).unwrap();
        let target = absolute_installer_target(temp.path()).unwrap();
        for loader in [
            crate::config::LoaderKind::Forge,
            crate::config::LoaderKind::NeoForge,
            crate::config::LoaderKind::Cleanroom,
        ] {
            let arguments = client_installer_arguments(&loader, &installer, &target).unwrap();
            assert_eq!(arguments[0], "-jar");
            assert_eq!(arguments[1], installer.as_os_str());
            assert_eq!(arguments[2], "--installClient");
            assert_eq!(arguments[3], target.as_os_str());
        }
        assert_eq!(target, std::path::absolute(temp.path()).unwrap());
        #[cfg(windows)]
        assert!(!target.to_string_lossy().starts_with(r"\\?\"));
    }

    #[test]
    fn fabric_client_does_not_use_the_jar_installer_command() {
        let error = client_installer_arguments(
            &crate::config::LoaderKind::Fabric,
            std::path::Path::new("fabric-installer.jar"),
            std::path::Path::new("run/client"),
        )
        .unwrap_err();

        assert!(matches!(
            error,
            RuntimeInstallError::UnsupportedJarClientInstaller {
                loader: crate::config::LoaderKind::Fabric
            }
        ));
    }

    #[test]
    fn installer_failure_details_never_returns_an_empty_message() {
        let mut command = if cfg!(windows) {
            let mut command = std::process::Command::new("cmd");
            command.args(["/C", "exit", "7"]);
            command
        } else {
            let mut command = std::process::Command::new("sh");
            command.args(["-c", "exit 7"]);
            command
        };
        let output = command.output().unwrap();

        let details = installer_failure_details(&output.status.to_string(), &[]);

        assert!(details.contains('7'));
    }

    #[test]
    fn installer_transcript_retains_only_the_recent_bounded_tail() {
        let mut transcript = InstallerTranscriptTail::default();
        for index in 0..(MAX_INSTALLER_TRANSCRIPT_LINES + 32) {
            transcript.push(format!("line-{index}"));
        }

        let lines = transcript.into_lines();

        assert_eq!(lines.len(), MAX_INSTALLER_TRANSCRIPT_LINES);
        assert!(lines.iter().map(String::len).sum::<usize>() <= MAX_INSTALLER_TRANSCRIPT_BYTES);
        assert_eq!(
            lines.last().unwrap(),
            &format!("line-{}", MAX_INSTALLER_TRANSCRIPT_LINES + 31)
        );
        assert_eq!(
            installer_failure_details("exit status: 7", &lines)
                .lines()
                .next(),
            Some("process exited with status exit status: 7")
        );
    }

    #[test]
    fn installer_output_is_streamed_before_exit_and_failure_keeps_both_streams() {
        let temp = tempfile::tempdir().unwrap();
        let marker = temp.path().join("process-finished");
        let acknowledgement = temp.path().join("callback-received");
        let spec = streaming_fixture_spec(temp.path(), &marker, &acknowledgement);
        assert!(spec.suppress_console_window);
        let mut lines = Vec::new();
        let mut observed_before_exit = false;

        let result = SystemInstallerProcessRunner
            .run(&spec, &mut |stream, line| {
                if line == "first-out" || line == "first-error" {
                    observed_before_exit |= !marker.exists();
                    std::fs::write(&acknowledgement, b"received").unwrap();
                }
                lines.push((stream, line));
            })
            .unwrap();

        assert!(observed_before_exit);
        assert!(marker.is_file());
        assert!(!result.success);
        assert!(lines.contains(&(RuntimeInstallStream::Stdout, "first-out".to_string())));
        assert!(lines.contains(&(RuntimeInstallStream::Stderr, "first-error".to_string())));
        let details = installer_failure_details(&result.status, &result.transcript);
        assert_eq!(
            details.lines().next(),
            Some(format!("process exited with status {}", result.status).as_str())
        );
        assert!(details.contains("stdout: first-out"));
        assert!(details.contains("stderr: first-error"));
    }

    #[test]
    fn installer_stream_fixture_process() {
        if std::env::var_os("MCMPU_INSTALLER_STREAM_FIXTURE").is_none() {
            return;
        }
        use std::io::Write as _;

        println!("first-out");
        std::io::stdout().flush().unwrap();
        eprintln!("first-error");
        std::io::stderr().flush().unwrap();
        let acknowledgement =
            std::path::PathBuf::from(std::env::var_os("MCMPU_INSTALLER_STREAM_ACK").unwrap());
        let marker =
            std::path::PathBuf::from(std::env::var_os("MCMPU_INSTALLER_STREAM_FINISHED").unwrap());
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !acknowledgement.is_file() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        std::fs::write(marker, b"finished").unwrap();
        std::process::exit(7);
    }

    fn streaming_fixture_spec(
        working_dir: &std::path::Path,
        marker: &std::path::Path,
        acknowledgement: &std::path::Path,
    ) -> InstallerProcessSpec {
        InstallerProcessSpec {
            executable: std::env::current_exe().unwrap(),
            args: vec![
                "--exact".into(),
                "project::runtime_install::tests::installer_stream_fixture_process".into(),
                "--nocapture".into(),
            ],
            env: vec![
                ("MCMPU_INSTALLER_STREAM_FIXTURE".into(), "1".into()),
                (
                    "MCMPU_INSTALLER_STREAM_ACK".into(),
                    acknowledgement.as_os_str().to_owned(),
                ),
                (
                    "MCMPU_INSTALLER_STREAM_FINISHED".into(),
                    marker.as_os_str().to_owned(),
                ),
            ],
            working_dir: working_dir.to_path_buf(),
            suppress_console_window: true,
        }
    }

    #[test]
    fn all_loader_installations_reuse_network_metadata_installers_and_process_outputs() {
        for loader in [
            LoaderKind::Forge,
            LoaderKind::Fabric,
            LoaderKind::NeoForge,
            LoaderKind::Cleanroom,
        ] {
            let temp = tempfile::tempdir().unwrap();
            let state = Arc::new(Mutex::new(FakeInstallState::new(loader.clone())));
            let downloader = Arc::new(CountingRuntimeDownloader::new(state.clone()));
            let runner = Arc::new(FakeRuntimeInstallerRunner::new(state));
            let locator = RuntimeProfileLocatorImpl {
                downloader: downloader.clone(),
                process_runner: runner.clone(),
                installer_proxy_arguments: Vec::new(),
                runtime_cache: temp.path().join(".mcmpu/cache/runtime"),
            };
            let config = runtime_config(loader.clone(), "1.0.0");
            let client = temp.path().join("run/client");
            let server = temp.path().join("run/server");
            std::fs::create_dir_all(&client).unwrap();
            std::fs::create_dir_all(&server).unwrap();
            let mut events = Vec::new();

            let first_profile = locator
                .prepare_client_profile(
                    &client,
                    &config,
                    std::path::Path::new("java"),
                    &mut |event: &RuntimeInstallProgress| events.push(event.clone()),
                )
                .unwrap();
            let first_server_args = locator
                .prepare_server_arguments(
                    &server,
                    &config,
                    std::path::Path::new("java"),
                    &mut |event| events.push(event.clone()),
                )
                .unwrap();
            let client_record: super::RuntimeInstallationRecord = serde_json::from_slice(
                &std::fs::read(locator.installation_record_path(super::RuntimeInstallSide::Client))
                    .unwrap(),
            )
            .unwrap();
            let server_record: super::RuntimeInstallationRecord = serde_json::from_slice(
                &std::fs::read(locator.installation_record_path(super::RuntimeInstallSide::Server))
                    .unwrap(),
            )
            .unwrap();
            assert_eq!(client_record.version, super::INSTALLATION_RECORD_VERSION);
            assert_eq!(client_record.side, super::RuntimeInstallSide::Client);
            assert_eq!(client_record.loader, loader);
            assert_eq!(client_record.minecraft, "1.20.1");
            assert_eq!(client_record.loader_version, "1.0.0");
            assert_eq!(
                client_record.client_profile.as_deref(),
                Some(first_profile.as_str())
            );
            assert!(server_record.server_launch_output.is_some());
            assert_eq!(server_record.side, super::RuntimeInstallSide::Server);
            let plan = crate::server::loader::plan_loader_install(
                &loader,
                &config.pack.minecraft_version,
                &config.pack.loader_version,
            )
            .unwrap();
            let (expected_url, expected_sha1) = super::installer_identity(&plan);
            let server_installer = server_record.installer.as_ref().unwrap();
            assert_eq!(server_installer.url, expected_url);
            assert_eq!(server_installer.sha1.len(), 40);
            if let Some(expected_sha1) = expected_sha1 {
                assert_eq!(server_installer.sha1, expected_sha1);
            }
            if loader == LoaderKind::Fabric {
                assert!(client_record.installer.is_none());
            } else {
                let client_installer = client_record.installer.as_ref().unwrap();
                assert_eq!(client_installer.url, expected_url);
                assert_eq!(client_installer.sha1, server_installer.sha1);
            }
            assert!(
                locator
                    .runtime_cache
                    .join("metadata/version_manifest_v2.json")
                    .is_file()
            );
            assert!(
                locator
                    .runtime_cache
                    .join("metadata/versions/1.20.1.json")
                    .is_file()
            );
            assert!(
                std::fs::read_dir(locator.runtime_cache.join("installers"))
                    .unwrap()
                    .any(|entry| entry
                        .unwrap()
                        .path()
                        .extension()
                        .is_some_and(|value| value == "jar"))
            );
            assert!(locator.runtime_cache.join("installers/checksums").is_dir());
            let downloads_after_first = downloader.count();
            let processes_after_first = runner.count();

            let second_profile = locator
                .prepare_client_profile(
                    &client,
                    &config,
                    std::path::Path::new("java"),
                    &mut |event| events.push(event.clone()),
                )
                .unwrap();
            let second_server_args = locator
                .prepare_server_arguments(
                    &server,
                    &config,
                    std::path::Path::new("java"),
                    &mut |event| events.push(event.clone()),
                )
                .unwrap();

            assert_eq!(second_profile, first_profile, "loader: {loader:?}");
            assert_eq!(second_server_args, first_server_args, "loader: {loader:?}");
            assert_eq!(
                downloader.count(),
                downloads_after_first,
                "loader: {loader:?}"
            );
            assert_eq!(runner.count(), processes_after_first, "loader: {loader:?}");
            assert!(events.iter().any(|event| matches!(
                event,
                RuntimeInstallProgress::InstallerOutput { line, .. }
                    if line.contains("validated client loader installation")
            )));
            assert!(events.iter().any(|event| matches!(
                event,
                RuntimeInstallProgress::InstallerOutput { line, .. }
                    if line.contains("validated server loader installation")
            )));
        }
    }

    #[test]
    fn standalone_cleanroom_profile_is_recorded_and_reused_without_reinstalling() {
        let temp = tempfile::tempdir().unwrap();
        let state = Arc::new(Mutex::new(FakeInstallState {
            loader: LoaderKind::Cleanroom,
            minecraft: "1.12.2".to_string(),
            loader_version: "0.6.4-alpha".to_string(),
        }));
        let downloader = Arc::new(CountingRuntimeDownloader::new(state.clone()));
        let runner = Arc::new(FakeRuntimeInstallerRunner::new(state));
        let locator = RuntimeProfileLocatorImpl {
            downloader: downloader.clone(),
            process_runner: runner.clone(),
            installer_proxy_arguments: Vec::new(),
            runtime_cache: temp.path().join(".mcmpu/cache/runtime"),
        };
        let mut config = runtime_config(LoaderKind::Cleanroom, "0.6.4-alpha");
        config.pack.minecraft_version = "1.12.2".to_string();
        let run_dir = temp.path().join("run/client");
        std::fs::create_dir_all(&run_dir).unwrap();

        let first_profile = locator
            .prepare_client_profile(&run_dir, &config, Path::new("java"), &mut |_| {})
            .unwrap();
        assert_eq!(first_profile, "1.12.2-Cleanroom-0.6.4-alpha");
        let record: super::RuntimeInstallationRecord = serde_json::from_slice(
            &std::fs::read(locator.installation_record_path(super::RuntimeInstallSide::Client))
                .unwrap(),
        )
        .unwrap();
        assert_eq!(
            record.client_profile.as_deref(),
            Some(first_profile.as_str())
        );
        let downloads_after_first = downloader.count();
        let processes_after_first = runner.count();
        let mut events = Vec::new();

        let second_profile = locator
            .prepare_client_profile(&run_dir, &config, Path::new("java"), &mut |event| {
                events.push(event.clone())
            })
            .unwrap();

        assert_eq!(second_profile, first_profile);
        assert_eq!(downloader.count(), downloads_after_first);
        assert_eq!(runner.count(), processes_after_first);
        assert!(events.iter().any(|event| matches!(
            event,
            RuntimeInstallProgress::InstallerOutput { line, .. }
                if line.contains("validated client loader installation")
        )));
    }

    #[test]
    fn missing_or_corrupt_output_reinstalls_only_that_side_from_cached_installer() {
        let temp = tempfile::tempdir().unwrap();
        let state = Arc::new(Mutex::new(FakeInstallState::new(LoaderKind::Cleanroom)));
        let downloader = Arc::new(CountingRuntimeDownloader::new(state.clone()));
        let runner = Arc::new(FakeRuntimeInstallerRunner::new(state));
        let locator = RuntimeProfileLocatorImpl {
            downloader: downloader.clone(),
            process_runner: runner.clone(),
            installer_proxy_arguments: Vec::new(),
            runtime_cache: temp.path().join(".mcmpu/cache/runtime"),
        };
        let config = runtime_config(LoaderKind::Cleanroom, "1.0.0");
        let client = temp.path().join("run/client");
        let server = temp.path().join("run/server");
        std::fs::create_dir_all(&client).unwrap();
        std::fs::create_dir_all(&server).unwrap();
        let profile = locator
            .prepare_client_profile(&client, &config, Path::new("java"), &mut |_| {})
            .unwrap();
        locator
            .prepare_server_arguments(&server, &config, Path::new("java"), &mut |_| {})
            .unwrap();
        let initial_downloads = downloader.count();
        let initial_processes = runner.count();

        std::fs::remove_file(super::profile_path(&client, &profile)).unwrap();
        locator
            .prepare_client_profile(&client, &config, Path::new("java"), &mut |_| {})
            .unwrap();
        assert_eq!(downloader.count(), initial_downloads);
        assert_eq!(runner.count(), initial_processes + 1);

        let server_jar = cleanroom_server_jar_path(&server, "1.0.0");
        std::fs::remove_file(server_jar).unwrap();
        locator
            .prepare_server_arguments(&server, &config, Path::new("java"), &mut |_| {})
            .unwrap();
        assert_eq!(downloader.count(), initial_downloads);
        assert_eq!(runner.count(), initial_processes + 2);

        std::fs::write(
            super::profile_path(&client, &profile),
            format!(
                r#"{{"id":"{profile}","inheritsFrom":"1.20.1","mainClass":"fixture.Main","libraries":"invalid"}}"#
            ),
        )
        .unwrap();
        locator
            .prepare_client_profile(&client, &config, Path::new("java"), &mut |_| {})
            .unwrap();
        assert_eq!(downloader.count(), initial_downloads);
        assert_eq!(runner.count(), initial_processes + 3);

        std::fs::write(cleanroom_server_jar_path(&server, "1.0.0"), b"not a jar").unwrap();
        locator
            .prepare_server_arguments(&server, &config, Path::new("java"), &mut |_| {})
            .unwrap();
        assert_eq!(downloader.count(), initial_downloads);
        assert_eq!(runner.count(), initial_processes + 4);
    }

    /// A loader switch must not leave the previous loader's jars on the classpath.
    #[test]
    fn loader_switch_clears_stale_library_and_version_directories() {
        let temp = tempfile::tempdir().unwrap();
        let state = Arc::new(Mutex::new(FakeInstallState::new(LoaderKind::Forge)));
        let downloader = Arc::new(CountingRuntimeDownloader::new(state.clone()));
        let runner = Arc::new(FakeRuntimeInstallerRunner::new(state.clone()));
        let locator = RuntimeProfileLocatorImpl {
            downloader: downloader.clone(),
            process_runner: runner.clone(),
            installer_proxy_arguments: Vec::new(),
            runtime_cache: temp.path().join(".mcmpu/cache/runtime"),
        };
        let run_dir = temp.path().join("run/client");
        std::fs::create_dir_all(&run_dir).unwrap();
        let forge = runtime_config(LoaderKind::Forge, "1.0.0");
        locator
            .prepare_client_profile(&run_dir, &forge, Path::new("java"), &mut |_| {})
            .unwrap();
        let stale = run_dir.join("libraries/stale/forge-only.jar");
        std::fs::create_dir_all(stale.parent().unwrap()).unwrap();
        std::fs::write(&stale, b"forge library").unwrap();

        // Re-running the same loader keeps the installation, matching the cache-hit path.
        locator
            .prepare_client_profile(&run_dir, &forge, Path::new("java"), &mut |_| {})
            .unwrap();
        assert!(stale.is_file());

        let fabric = runtime_config(LoaderKind::Fabric, "1.0.0");
        locator
            .prepare_client_profile(&run_dir, &fabric, Path::new("java"), &mut |_| {})
            .unwrap();

        assert!(!stale.exists());
        assert!(run_dir.join(".mcmpu-libraries.json").is_file());
        assert!(
            !run_dir
                .join("versions")
                .join("1.0.0")
                .join("stale")
                .exists()
        );
    }

    /// A Cleanroom version bump keeps the libraries the two builds share.
    ///
    /// The switch used to clear `libraries/`, which made the next start re-download every jar even
    /// though the new profile still asked for most of them.
    #[test]
    fn cleanroom_version_bump_keeps_existing_libraries() {
        let temp = tempfile::tempdir().unwrap();
        let state = Arc::new(Mutex::new(FakeInstallState {
            loader: LoaderKind::Cleanroom,
            minecraft: "1.12.2".to_string(),
            loader_version: "0.6.13-alpha".to_string(),
        }));
        let downloader = Arc::new(CountingRuntimeDownloader::new(state.clone()));
        let runner = Arc::new(FakeRuntimeInstallerRunner::new(state.clone()));
        let locator = RuntimeProfileLocatorImpl {
            downloader,
            process_runner: runner,
            installer_proxy_arguments: Vec::new(),
            runtime_cache: temp.path().join(".mcmpu/cache/runtime"),
        };
        let run_dir = temp.path().join("run/client");
        std::fs::create_dir_all(&run_dir).unwrap();
        let mut old = runtime_config(LoaderKind::Cleanroom, "0.6.13-alpha");
        old.pack.minecraft_version = "1.12.2".to_string();
        locator
            .prepare_client_profile(&run_dir, &old, Path::new("java"), &mut |_| {})
            .unwrap();
        // A library both builds reference, and one the new build no longer asks for.
        let shared = run_dir.join("libraries/org/ow2/asm/asm/9.10.1/asm-9.10.1.jar");
        std::fs::create_dir_all(shared.parent().unwrap()).unwrap();
        std::fs::write(&shared, b"shared library").unwrap();
        let stale = run_dir.join("libraries/only-in-old/old.jar");
        std::fs::create_dir_all(stale.parent().unwrap()).unwrap();
        std::fs::write(&stale, b"old only").unwrap();

        // The stub installer writes whatever version its state names, so move it forward.
        *state.lock().unwrap() = FakeInstallState {
            loader: LoaderKind::Cleanroom,
            minecraft: "1.12.2".to_string(),
            loader_version: "0.7.2-alpha".to_string(),
        };
        let mut next = runtime_config(LoaderKind::Cleanroom, "0.7.2-alpha");
        next.pack.minecraft_version = "1.12.2".to_string();
        locator
            .prepare_client_profile(&run_dir, &next, Path::new("java"), &mut |_| {})
            .unwrap();

        // The shared jar survives; the version profiles are still rebuilt.
        assert!(
            shared.is_file(),
            "a shared library must not be deleted on a version bump"
        );
        assert!(
            stale.is_file(),
            "a leftover library is harmless: it is checksum-verified before use"
        );
        assert!(run_dir.join(".mcmpu-libraries.json").is_file());
    }

    #[test]
    fn loader_version_change_invalidates_installation_but_reuses_minecraft_metadata() {
        let temp = tempfile::tempdir().unwrap();
        let state = Arc::new(Mutex::new(FakeInstallState::new(LoaderKind::Forge)));
        let downloader = Arc::new(CountingRuntimeDownloader::new(state.clone()));
        let runner = Arc::new(FakeRuntimeInstallerRunner::new(state.clone()));
        let locator = RuntimeProfileLocatorImpl {
            downloader: downloader.clone(),
            process_runner: runner.clone(),
            installer_proxy_arguments: Vec::new(),
            runtime_cache: temp.path().join(".mcmpu/cache/runtime"),
        };
        let run_dir = temp.path().join("run/client");
        std::fs::create_dir_all(&run_dir).unwrap();
        let first = runtime_config(LoaderKind::Forge, "1.0.0");
        locator
            .prepare_client_profile(&run_dir, &first, Path::new("java"), &mut |_| {})
            .unwrap();
        let manifest_requests = downloader.url_count(VERSION_MANIFEST_URL);
        let first_processes = runner.count();
        state.lock().unwrap().loader_version = "2.0.0".to_string();
        let second = runtime_config(LoaderKind::Forge, "2.0.0");

        locator
            .prepare_client_profile(&run_dir, &second, Path::new("java"), &mut |_| {})
            .unwrap();

        assert_eq!(runner.count(), first_processes + 1);
        assert_eq!(
            downloader.url_count(VERSION_MANIFEST_URL),
            manifest_requests
        );
        assert!(
            downloader
                .requested_urls()
                .iter()
                .any(|url| url.contains("2.0.0") && url.ends_with(".jar.sha1"))
        );
    }

    #[test]
    fn missing_server_library_invalidates_forge_installation_record() {
        let temp = tempfile::tempdir().unwrap();
        let state = Arc::new(Mutex::new(FakeInstallState::new(LoaderKind::Forge)));
        let downloader = Arc::new(CountingRuntimeDownloader::new(state.clone()));
        let runner = Arc::new(FakeRuntimeInstallerRunner::new(state));
        let locator = RuntimeProfileLocatorImpl {
            downloader: downloader.clone(),
            process_runner: runner.clone(),
            installer_proxy_arguments: Vec::new(),
            runtime_cache: temp.path().join(".mcmpu/cache/runtime"),
        };
        let config = runtime_config(LoaderKind::Forge, "1.0.0");
        let server = temp.path().join("run/server");
        std::fs::create_dir_all(&server).unwrap();

        locator
            .prepare_server_arguments(&server, &config, Path::new("java"), &mut |_| {})
            .unwrap();
        let downloads = downloader.count();
        let processes = runner.count();
        std::fs::remove_file(server.join("libraries/fixture/server-lib.jar")).unwrap();

        locator
            .prepare_server_arguments(&server, &config, Path::new("java"), &mut |_| {})
            .unwrap();

        assert_eq!(downloader.count(), downloads);
        assert_eq!(runner.count(), processes + 1);
        assert!(server.join("libraries/fixture/server-lib.jar").is_file());
    }

    #[test]
    fn concurrent_client_preparation_runs_installer_once() {
        let temp = tempfile::tempdir().unwrap();
        let state = Arc::new(Mutex::new(FakeInstallState::new(LoaderKind::Forge)));
        let downloader = Arc::new(CountingRuntimeDownloader::new(state.clone()));
        let runner = Arc::new(FakeRuntimeInstallerRunner::new(state));
        let locator = Arc::new(RuntimeProfileLocatorImpl {
            downloader,
            process_runner: runner.clone(),
            installer_proxy_arguments: Vec::new(),
            runtime_cache: temp.path().join(".mcmpu/cache/runtime"),
        });
        let config = Arc::new(runtime_config(LoaderKind::Forge, "1.0.0"));
        let run_dir = temp.path().join("run/client");
        std::fs::create_dir_all(&run_dir).unwrap();
        let barrier = Arc::new(std::sync::Barrier::new(2));

        let profiles = std::thread::scope(|scope| {
            let mut handles = Vec::new();
            for _ in 0..2 {
                let locator = Arc::clone(&locator);
                let config = Arc::clone(&config);
                let run_dir = run_dir.clone();
                let barrier = Arc::clone(&barrier);
                handles.push(scope.spawn(move || {
                    barrier.wait();
                    locator
                        .prepare_client_profile(&run_dir, &config, Path::new("java"), &mut |_| {})
                        .unwrap()
                }));
            }
            handles
                .into_iter()
                .map(|handle| handle.join().unwrap())
                .collect::<Vec<_>>()
        });

        assert_eq!(profiles[0], profiles[1]);
        assert_eq!(runner.count(), 1);
    }

    #[derive(Debug)]
    struct FakeInstallState {
        loader: LoaderKind,
        minecraft: String,
        loader_version: String,
    }

    impl FakeInstallState {
        fn new(loader: LoaderKind) -> Self {
            Self {
                loader,
                minecraft: "1.20.1".to_string(),
                loader_version: "1.0.0".to_string(),
            }
        }
    }

    struct CountingRuntimeDownloader {
        state: Arc<Mutex<FakeInstallState>>,
        urls: Mutex<Vec<String>>,
        installer: Vec<u8>,
    }

    impl CountingRuntimeDownloader {
        fn new(state: Arc<Mutex<FakeInstallState>>) -> Self {
            Self {
                state,
                urls: Mutex::new(Vec::new()),
                installer: b"fixture loader installer".to_vec(),
            }
        }

        fn count(&self) -> usize {
            self.urls.lock().unwrap().len()
        }

        fn url_count(&self, expected: &str) -> usize {
            self.urls
                .lock()
                .unwrap()
                .iter()
                .filter(|url| url.as_str() == expected)
                .count()
        }

        fn requested_urls(&self) -> Vec<String> {
            self.urls.lock().unwrap().clone()
        }
    }

    impl RuntimeInstallDownloader for CountingRuntimeDownloader {
        fn download(&self, url: &str) -> Result<Vec<u8>, RuntimeInstallError> {
            self.urls.lock().unwrap().push(url.to_string());
            let state = self.state.lock().unwrap();
            if url == VERSION_MANIFEST_URL {
                return Ok(format!(
                    r#"{{"versions":[{{"id":"{}","url":"https://fixture.invalid/version.json"}}]}}"#,
                    state.minecraft
                )
                .into_bytes());
            }
            if url == "https://fixture.invalid/version.json" {
                return Ok(format!(r#"{{"id":"{}"}}"#, state.minecraft).into_bytes());
            }
            if url.starts_with("https://meta.fabricmc.net/v2/versions/loader/") {
                return Ok(format!(
                    r#"{{"id":"fabric-loader-{}-{}","inheritsFrom":"{}","mainClass":"fixture.Main"}}"#,
                    state.loader_version, state.minecraft, state.minecraft
                )
                .into_bytes());
            }
            if url.ends_with(".sha1") {
                return Ok(format!("{:x}\n", sha1::Sha1::digest(&self.installer)).into_bytes());
            }
            if url.ends_with(".jar") {
                return Ok(self.installer.clone());
            }
            Err(RuntimeInstallError::InvalidProfile {
                reason: format!("unexpected fixture URL '{url}'"),
            })
        }
    }

    struct FakeRuntimeInstallerRunner {
        state: Arc<Mutex<FakeInstallState>>,
        runs: AtomicUsize,
    }

    impl FakeRuntimeInstallerRunner {
        fn new(state: Arc<Mutex<FakeInstallState>>) -> Self {
            Self {
                state,
                runs: AtomicUsize::new(0),
            }
        }

        fn count(&self) -> usize {
            self.runs.load(Ordering::SeqCst)
        }
    }

    impl InstallerProcessRunner for FakeRuntimeInstallerRunner {
        fn run(
            &self,
            spec: &InstallerProcessSpec,
            _progress: &mut dyn FnMut(RuntimeInstallStream, String),
        ) -> Result<InstallerProcessResult, super::InstallerProcessError> {
            self.runs.fetch_add(1, Ordering::SeqCst);
            let state = self.state.lock().unwrap();
            let client = spec
                .args
                .iter()
                .any(|argument| argument == std::ffi::OsStr::new("--installClient"));
            if client {
                let id = match state.loader {
                    LoaderKind::Cleanroom => {
                        format!("{}-Cleanroom-{}", state.minecraft, state.loader_version)
                    }
                    _ => format!("fixture-{}", state.loader_version),
                };
                let profile_path = super::profile_path(&spec.working_dir, &id);
                std::fs::create_dir_all(profile_path.parent().unwrap()).unwrap();
                let profile = if state.loader == LoaderKind::Cleanroom {
                    serde_json::json!({
                        "id": id,
                        "mainClass": "top.outlands.foundation.boot.Foundation"
                    })
                } else {
                    serde_json::json!({
                        "id": id,
                        "inheritsFrom": state.minecraft,
                        "mainClass": "fixture.Main"
                    })
                };
                std::fs::write(profile_path, serde_json::to_vec(&profile).unwrap()).unwrap();
            } else {
                write_fake_server_output(&spec.working_dir, &state);
            }
            Ok(InstallerProcessResult {
                success: true,
                status: "exit status: 0".to_string(),
                transcript: Vec::new(),
            })
        }
    }

    fn write_fake_server_output(run_dir: &Path, state: &FakeInstallState) {
        match state.loader {
            LoaderKind::Forge | LoaderKind::NeoForge => {
                let path = server_launch_output_path(
                    run_dir,
                    &state.loader,
                    &state.minecraft,
                    &state.loader_version,
                    cfg!(windows),
                );
                std::fs::create_dir_all(path.parent().unwrap()).unwrap();
                let library = run_dir.join("libraries/fixture/server-lib.jar");
                std::fs::create_dir_all(library.parent().unwrap()).unwrap();
                std::fs::write(library, b"fixture library").unwrap();
                std::fs::write(
                    path,
                    "-DlegacyClassPath=libraries/fixture/server-lib.jar\n-Dfixture=true\n",
                )
                .unwrap();
            }
            LoaderKind::Fabric => {
                write_executable_jar(&run_dir.join("fabric-server-launch.jar"));
            }
            LoaderKind::Cleanroom => {
                write_executable_jar(&cleanroom_server_jar_path(run_dir, &state.loader_version));
            }
        }
    }

    fn write_executable_jar(path: &Path) {
        let file = std::fs::File::create(path).unwrap();
        let mut archive = zip::ZipWriter::new(file);
        archive
            .start_file(
                "META-INF/MANIFEST.MF",
                zip::write::SimpleFileOptions::default(),
            )
            .unwrap();
        archive
            .write_all(b"Manifest-Version: 1.0\nMain-Class: fixture.Main\n")
            .unwrap();
        archive.finish().unwrap();
    }

    fn runtime_config(loader: LoaderKind, loader_version: &str) -> Config {
        Config {
            language: "en-US".to_string(),
            pack: PackConfig {
                name: "Runtime fixture".to_string(),
                version: "1.0.0".to_string(),
                authors: vec!["Fixture".to_string()],
                minecraft_version: "1.20.1".to_string(),
                loader,
                loader_version: loader_version.to_string(),
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

    fn write_cleanroom_server_jar(path: &std::path::Path, manifest: &[u8]) {
        let file = std::fs::File::create(path).unwrap();
        let mut archive = zip::ZipWriter::new(file);
        archive
            .start_file(
                "META-INF/MANIFEST.MF",
                zip::write::SimpleFileOptions::default(),
            )
            .unwrap();
        archive.write_all(manifest).unwrap();
        archive.finish().unwrap();
    }

    /// Builds an installer jar holding the same `version.json` shape Cleanroom ships.
    fn write_installer_with_libraries(path: &std::path::Path, libraries: serde_json::Value) {
        let file = std::fs::File::create(path).unwrap();
        let mut archive = zip::ZipWriter::new(file);
        archive
            .start_file("version.json", zip::write::SimpleFileOptions::default())
            .unwrap();
        archive
            .write_all(
                serde_json::to_string(&serde_json::json!({ "libraries": libraries }))
                    .unwrap()
                    .as_bytes(),
            )
            .unwrap();
        archive.finish().unwrap();
    }

    struct LibraryDownloader {
        payloads: std::collections::BTreeMap<String, Vec<u8>>,
        urls: Mutex<Vec<String>>,
    }

    impl LibraryDownloader {
        fn new(payloads: std::collections::BTreeMap<String, Vec<u8>>) -> Self {
            Self {
                payloads,
                urls: Mutex::new(Vec::new()),
            }
        }

        fn requested_urls(&self) -> Vec<String> {
            self.urls.lock().unwrap().clone()
        }
    }

    impl RuntimeInstallDownloader for LibraryDownloader {
        fn download(&self, url: &str) -> Result<Vec<u8>, RuntimeInstallError> {
            self.urls.lock().unwrap().push(url.to_string());
            self.payloads
                .get(url)
                .cloned()
                .ok_or_else(|| RuntimeInstallError::InvalidProfile {
                    reason: format!("unexpected fixture URL '{url}'"),
                })
        }
    }

    fn library_payload(url: &str, bytes: &[u8]) -> serde_json::Value {
        serde_json::json!({
            "name": "fixture:library:1.0",
            "downloads": {
                "artifact": {
                    "path": url.rsplit('/').next().unwrap(),
                    "url": url,
                    "sha1": format!("{:x}", sha1::Sha1::digest(bytes)),
                    "size": bytes.len(),
                }
            }
        })
    }

    #[test]
    fn installer_libraries_are_fetched_through_the_shared_downloader() {
        let temp = tempfile::tempdir().unwrap();
        let url = "https://fixture.invalid/com/example/library-1.0.jar";
        let installer = temp.path().join("installer.jar");
        write_installer_with_libraries(
            &installer,
            serde_json::json!([library_payload(url, b"payload")]),
        );

        let downloader = LibraryDownloader::new(std::collections::BTreeMap::from([(
            url.to_string(),
            b"payload".to_vec(),
        )]));
        let run_dir = temp.path().join("run/client");
        std::fs::create_dir_all(&run_dir).unwrap();
        let mut events = Vec::new();

        preseed_installer_libraries(
            &downloader,
            &run_dir,
            &installer,
            "1.12.2",
            &mut |event: &RuntimeInstallProgress| events.push(event.clone()),
        )
        .unwrap();

        assert_eq!(downloader.requested_urls(), vec![url.to_string()]);
        assert_eq!(
            std::fs::read(run_dir.join("libraries/library-1.0.jar")).unwrap(),
            b"payload"
        );
        assert!(events.iter().any(|event| matches!(
            event,
            RuntimeInstallProgress::InstallingLibraries { total: 1 }
        )));
        assert!(events.iter().any(|event| matches!(
            event,
            RuntimeInstallProgress::LibraryReady {
                completed: 1,
                total: 1,
                ..
            }
        )));

        // Everything is verified now, so the second pass downloads nothing at all.
        let before = downloader.requested_urls().len();
        preseed_installer_libraries(&downloader, &run_dir, &installer, "1.12.2", &mut |_| {})
            .unwrap();
        assert_eq!(downloader.requested_urls().len(), before);
    }

    #[test]
    fn installer_libraries_without_a_url_are_left_to_the_installer() {
        let temp = tempfile::tempdir().unwrap();
        let installer = temp.path().join("installer.jar");
        write_installer_with_libraries(
            &installer,
            serde_json::json!([
                // The loader's own jar: the installer produces it, there is nothing to fetch.
                {
                    "name": "com.cleanroommc:cleanroom:0.6.13-alpha",
                    "downloads": { "artifact": { "path": "com/cleanroommc/cleanroom/0.6.13-alpha/cleanroom-0.6.13-alpha.jar", "url": "", "sha1": "aa", "size": 1 } }
                },
                // A path that would escape the libraries directory must not be touched either.
                {
                    "name": "fixture:escape:1.0",
                    "downloads": { "artifact": { "path": "../../outside.jar", "url": "https://fixture.invalid/outside.jar", "sha1": "bb", "size": 1 } }
                }
            ]),
        );

        let downloader = LibraryDownloader::new(std::collections::BTreeMap::new());
        let run_dir = temp.path().join("run/server");
        std::fs::create_dir_all(&run_dir).unwrap();
        preseed_installer_libraries(&downloader, &run_dir, &installer, "1.12.2", &mut |_| {})
            .unwrap();

        assert!(downloader.requested_urls().is_empty());
        assert!(!temp.path().join("outside.jar").exists());
    }

    /// The reported failure: a reused installation must restore the library that only exists
    /// inside the installer jar, instead of letting the launcher fetch an equally named file
    /// from a repository and hit a checksum mismatch.
    #[test]
    fn reuse_restores_the_library_bundled_inside_the_installer() {
        let temp = tempfile::tempdir().unwrap();
        let run_dir = temp.path().join("run/client");
        std::fs::create_dir_all(run_dir.join("libraries")).unwrap();

        // A jar that carries one bundled library, in the installer's own `maven/` layout.
        let payload = b"cleanroom loader payload".to_vec();
        let installer = temp.path().join("installer.jar");
        let file = std::fs::File::create(&installer).unwrap();
        let mut archive = zip::ZipWriter::new(file);
        archive
            .start_file(
                "maven/com/cleanroommc/cleanroom/0.6.13-alpha/cleanroom-0.6.13-alpha.jar",
                zip::write::SimpleFileOptions::default(),
            )
            .unwrap();
        archive.write_all(&payload).unwrap();
        archive.finish().unwrap();

        let mut events = Vec::new();
        restore_installer_bundled_libraries(
            &installer,
            &run_dir,
            &LoaderKind::Cleanroom,
            &mut |event| events.push(event.clone()),
        )
        .unwrap();

        let restored = run_dir
            .join("libraries/com/cleanroommc/cleanroom/0.6.13-alpha/cleanroom-0.6.13-alpha.jar");
        assert_eq!(std::fs::read(&restored).unwrap(), payload);
        assert!(events.iter().any(|event| matches!(
            event,
            RuntimeInstallProgress::InstallerOutput { line, .. }
                if line.contains("restored bundled")
        )));

        // A second pass over an intact file rewrites nothing.
        let modified = std::fs::metadata(&restored).unwrap().modified().unwrap();
        restore_installer_bundled_libraries(
            &installer,
            &run_dir,
            &LoaderKind::Cleanroom,
            &mut |_| {},
        )
        .unwrap();
        assert_eq!(
            std::fs::metadata(&restored).unwrap().modified().unwrap(),
            modified
        );
    }

    /// Every library in one batch needs its own transfer task id: the transfer layer rejects a
    /// batch that repeats one, which previously made the whole preseed fail and left the work to
    /// the installer.
    #[test]
    fn installer_library_requests_use_unique_task_ids() {
        let requests = [
            InstallerLibraryRequest {
                url: "https://fixture.invalid/a.jar".to_string(),
                target: PathBuf::from("libraries/fixture/a.jar"),
                sha1: "a".to_string(),
            },
            InstallerLibraryRequest {
                url: "https://fixture.invalid/b.jar".to_string(),
                target: PathBuf::from("libraries/fixture/b.jar"),
                sha1: "b".to_string(),
            },
        ];
        let ids = requests
            .iter()
            .map(|request| request.target.to_string_lossy().to_string())
            .collect::<Vec<_>>();
        let unique = ids.iter().collect::<std::collections::BTreeSet<_>>();
        assert_eq!(unique.len(), ids.len(), "task ids must be unique: {ids:?}");
        assert!(ids.iter().all(|id| !id.is_empty()));
    }

    /// Duplicated targets must collapse to one entry: a transfer batch rejects a repeated task
    /// id, which made the whole preseed fail and left every library to the installer.
    #[test]
    fn installer_libraries_collapse_duplicate_targets() {
        let temp = tempfile::tempdir().unwrap();
        let installer = temp.path().join("installer.jar");
        // Two entries for one target, as the real profile has for text2speech.
        write_installer_with_libraries(
            &installer,
            serde_json::json!([
                {
                    "name": "com.mojang:text2speech:1.10.3",
                    "downloads": { "artifact": { "url": "https://fixture.invalid/a.jar", "sha1": "aa", "size": 1 } }
                },
                {
                    "name": "com.mojang:text2speech:1.10.3",
                    "downloads": { "artifact": { "url": "https://fixture.invalid/a.jar", "sha1": "aa", "size": 1 } }
                }
            ]),
        );

        let (libraries, client) = read_installer_preseed_items(&installer, "1.12.2").unwrap();
        assert_eq!(
            libraries.len(),
            1,
            "duplicate targets must collapse: {libraries:?}"
        );
        assert!(client.is_none());
        assert_eq!(
            libraries[0].path,
            "com/mojang/text2speech/1.10.3/text2speech-1.10.3.jar"
        );
    }

    /// The client jar the installer would download is read from the same profile, so the project
    /// downloader can fetch it before a slow Mojang connection aborts the installation.
    #[test]
    fn installer_client_jar_is_read_from_the_profile() {
        let temp = tempfile::tempdir().unwrap();
        let installer = temp.path().join("installer.jar");
        let file = std::fs::File::create(&installer).unwrap();
        let mut archive = zip::ZipWriter::new(file);
        archive
            .start_file("version.json", zip::write::SimpleFileOptions::default())
            .unwrap();
        archive
            .write_all(
                serde_json::to_string(&serde_json::json!({
                    "libraries": [],
                    "downloads": {
                        "client": {
                            "url": "https://piston-data.mojang.com/v1/objects/abc/client.jar",
                            "sha1": "ABC123",
                            "size": 1234
                        }
                    }
                }))
                .unwrap()
                .as_bytes(),
            )
            .unwrap();
        archive.finish().unwrap();

        let (libraries, client) = read_installer_preseed_items(&installer, "1.12.2").unwrap();
        assert!(libraries.is_empty());
        let client = client.expect("client jar entry is read");
        assert_eq!(client.sha1, "abc123", "digest is normalised to lower case");
        assert_eq!(client.size, Some(1234));
        assert!(client.url.ends_with("/client.jar"));
    }

    #[test]
    fn an_unreadable_installer_does_not_stop_the_installation() {
        let temp = tempfile::tempdir().unwrap();
        let installer = temp.path().join("installer.jar");
        // The stub installers used by the other tests are not zips; reading them must fail
        // cleanly so the caller can carry on with the real installer.
        std::fs::write(&installer, b"not an archive").unwrap();

        let downloader = LibraryDownloader::new(std::collections::BTreeMap::new());
        let run_dir = temp.path().join("run/client");
        std::fs::create_dir_all(&run_dir).unwrap();
        assert!(matches!(
            preseed_installer_libraries(&downloader, &run_dir, &installer, "1.12.2", &mut |_| {}),
            Err(RuntimeInstallError::InvalidInstallerArchive { .. })
        ));
    }

    /// The Cleanroom fast path publishes the installer's own profile and never starts the JVM.
    ///
    /// This is what removes the installer's intermediate downloads from the client install: the
    /// embedded `version.json` is the finished profile, so there is nothing left for the installer
    /// to do.
    #[test]
    fn cleanroom_client_profile_is_published_without_running_the_installer() {
        let temp = tempfile::tempdir().unwrap();
        let state = Arc::new(Mutex::new(FakeInstallState {
            loader: LoaderKind::Cleanroom,
            minecraft: "1.12.2".to_string(),
            loader_version: "0.6.13-alpha".to_string(),
        }));
        let downloader = Arc::new(CountingRuntimeDownloader::new(state.clone()));
        let runner = Arc::new(FakeRuntimeInstallerRunner::new(state));
        let locator = RuntimeProfileLocatorImpl {
            downloader: downloader.clone(),
            process_runner: runner.clone(),
            installer_proxy_arguments: Vec::new(),
            runtime_cache: temp.path().join(".mcmpu/cache/runtime"),
        };
        // A real installer archive: the fast path reads `version.json` out of it.
        let installer_jar = temp.path().join("cleanroom-installer.jar");
        let file = std::fs::File::create(&installer_jar).unwrap();
        let mut archive = zip::ZipWriter::new(file);
        archive
            .start_file("version.json", zip::write::SimpleFileOptions::default())
            .unwrap();
        archive
            .write_all(
                serde_json::to_vec(&serde_json::json!({
                    "id": "1.12.2-Cleanroom-0.6.13-alpha",
                    "mainClass": "top.outlands.foundation.boot.Foundation",
                    "downloads": {
                        "client": { "url": "https://fixture.invalid/client.jar",
                                    "sha1": "cc", "size": 2 }
                    },
                    "libraries": [
                        { "name": "com.cleanroommc:cleanroom:0.6.13-alpha",
                          "downloads": { "artifact": { "url": "https://fixture.invalid/cleanroom.jar",
                                                       "sha1": "aa", "size": 1 } } }
                    ]
                }))
                .unwrap()
                .as_slice(),
            )
            .unwrap();
        // The installer also carries the Cleanroom jar itself, whose profile entry has no URL:
        // running the installer used to unpack it, so the fast path must do the same.
        let bundled = b"bundled cleanroom jar";
        archive
            .start_file(
                "maven/com/cleanroommc/cleanroom/0.6.13-alpha/cleanroom-0.6.13-alpha.jar",
                zip::write::SimpleFileOptions::default(),
            )
            .unwrap();
        archive.write_all(bundled).unwrap();
        archive.finish().unwrap();

        let mut config = runtime_config(LoaderKind::Cleanroom, "0.6.13-alpha");
        config.pack.minecraft_version = "1.12.2".to_string();
        let run_dir = temp.path().join("run/client");
        std::fs::create_dir_all(&run_dir).unwrap();

        let published = locator
            .write_cleanroom_profile_from_installer(&run_dir, &config, &installer_jar, &mut |_| {})
            .unwrap();

        assert_eq!(published.as_deref(), Some("1.12.2-Cleanroom-0.6.13-alpha"));
        let written = super::profile_path(&run_dir, "1.12.2-Cleanroom-0.6.13-alpha");
        assert!(
            written.is_file(),
            "profile must be published to the run directory"
        );
        // The bundled jar is unpacked exactly as the installer would have done it.
        assert_eq!(
            std::fs::read(run_dir.join(
                "libraries/com/cleanroommc/cleanroom/0.6.13-alpha/cleanroom-0.6.13-alpha.jar"
            ))
            .unwrap(),
            bundled
        );
        // The client jar is anchored to the vanilla slot rather than to the profile id.
        let profile: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&written).unwrap()).unwrap();
        assert_eq!(
            profile["downloads"]["client"]["path"],
            "versions/1.12.2/1.12.2.jar"
        );
        // Nothing was downloaded and no JVM was started.
        assert_eq!(downloader.count(), 0);
        assert_eq!(runner.count(), 0);
    }

    /// A profile whose bundled Cleanroom is not the requested version is refused, so the caller
    /// falls back to running the installer instead of publishing the wrong build.
    #[test]
    fn cleanroom_fast_path_refuses_a_mismatched_embedded_profile() {
        let temp = tempfile::tempdir().unwrap();
        let installer_jar = temp.path().join("cleanroom-installer.jar");
        let file = std::fs::File::create(&installer_jar).unwrap();
        let mut archive = zip::ZipWriter::new(file);
        archive
            .start_file("version.json", zip::write::SimpleFileOptions::default())
            .unwrap();
        archive
            .write_all(
                serde_json::to_vec(&serde_json::json!({
                    "id": "1.12.2-Cleanroom-0.5.0-alpha",
                    "mainClass": "top.outlands.foundation.boot.Foundation",
                    "libraries": [
                        { "name": "com.cleanroommc:cleanroom:0.5.0-alpha",
                          "downloads": { "artifact": { "url": "https://fixture.invalid/cleanroom.jar",
                                                       "sha1": "aa", "size": 1 } } }
                    ]
                }))
                .unwrap()
                .as_slice(),
            )
            .unwrap();
        archive.finish().unwrap();

        let mut config = runtime_config(LoaderKind::Cleanroom, "0.6.13-alpha");
        config.pack.minecraft_version = "1.12.2".to_string();
        let run_dir = temp.path().join("run/client");
        std::fs::create_dir_all(&run_dir).unwrap();
        let downloader = Arc::new(CountingRuntimeDownloader::new(Arc::new(Mutex::new(
            FakeInstallState {
                loader: LoaderKind::Cleanroom,
                minecraft: "1.12.2".to_string(),
                loader_version: "0.6.13-alpha".to_string(),
            },
        ))));
        let locator = RuntimeProfileLocatorImpl {
            downloader,
            process_runner: Arc::new(FakeRuntimeInstallerRunner::new(Arc::new(Mutex::new(
                FakeInstallState {
                    loader: LoaderKind::Cleanroom,
                    minecraft: "1.12.2".to_string(),
                    loader_version: "0.6.13-alpha".to_string(),
                },
            )))),
            installer_proxy_arguments: Vec::new(),
            runtime_cache: temp.path().join(".mcmpu/cache/runtime"),
        };

        let published = locator
            .write_cleanroom_profile_from_installer(&run_dir, &config, &installer_jar, &mut |_| {})
            .unwrap();
        assert!(
            published.is_none(),
            "a mismatched build must not be published"
        );
    }

    /// Mirrors are appended after the original URL, and a URL with no mirror keeps one candidate.
    #[test]
    fn artifact_candidates_append_mirrors_and_keep_the_original_first() {
        let maven = "https://repo.maven.apache.org/maven2/org/ow2/asm/asm/9.10.1/asm-9.10.1.jar";
        let candidates = super::artifact_candidate_urls(maven);
        assert_eq!(candidates[0], maven);
        assert!(candidates.len() > 1, "a known repository gains a mirror");
        assert!(candidates[1].starts_with("https://maven.aliyun.com/repository/public/"));

        let unknown = "https://example.invalid/some/artifact.jar";
        assert_eq!(
            super::artifact_candidate_urls(unknown),
            vec![unknown.to_string()]
        );
    }
}
