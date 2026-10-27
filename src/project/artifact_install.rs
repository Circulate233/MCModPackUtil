use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use sha1::{Digest, Sha1};
use thiserror::Error;

use crate::project::launcher::{LaunchArtifact, LaunchDescriptor, NativeArtifact};

const ARTIFACT_CACHE_LOCK_TIMEOUT: Duration = Duration::from_secs(120);
const MIN_ARTIFACT_WORKERS: usize = 4;
const MAX_ARTIFACT_WORKERS: usize = 32;
/// Interval between live download-progress lines.
const ARTIFACT_DOWNLOAD_PROGRESS_INTERVAL: Duration = Duration::from_millis(500);

/// Explains whether an installed runtime artifact was reused or fetched during this run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArtifactInstallSource {
    /// The verified artifact already existed in the run directory.
    TargetHit,
    /// The verified artifact already existed in the runtime cache.
    CacheHit,
    /// The cache entry was absent or invalid and was downloaded again.
    Downloaded,
}

/// Incremental status emitted while Minecraft libraries, natives, and assets are prepared.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ArtifactInstallProgress {
    /// The descriptor supplied this many known artifacts before asset-index expansion.
    Starting { total: usize },
    /// An asset index exposed more individual asset objects, increasing the final total.
    TotalUpdated { total: usize },
    /// One artifact is verified and present in the run directory.
    ArtifactReady {
        /// One-based completion count.
        completed: usize,
        /// Current total count, including objects discovered in the asset index.
        total: usize,
        /// Relative path within the run and cache directories.
        path: String,
        /// Whether this run reused the target, reused the cache, or downloaded the artifact.
        source: ArtifactInstallSource,
    },
    /// Bytes are arriving for the batch of artifacts currently downloading.
    ///
    /// Files finish one by one, so their `ArtifactReady` events would leave long downloads with
    /// no sign of life; this reports the bytes that moved in between.
    Downloading {
        /// Bytes received so far for the artifacts downloading now.
        transferred_bytes: u64,
        /// Expected bytes for those artifacts, as far as the descriptors know.
        total_bytes: u64,
        /// Requests currently in flight across the shared network engine.
        active_requests: usize,
    },
}

/// Rate limit for live download progress so a fast batch cannot flood the console.
#[derive(Default)]
struct DownloadProgressThrottle {
    last_emit: Option<Instant>,
}

/// Emits at most one download-progress line per interval, always allowing the first update.
fn emit_download_progress(
    progress: &mut dyn FnMut(&ArtifactInstallProgress),
    throttle: &mut DownloadProgressThrottle,
    update: ArtifactDownloadProgress,
) {
    if update.total_bytes == 0 {
        return;
    }
    let now = Instant::now();
    if let Some(last) = throttle.last_emit
        && now.duration_since(last) < ARTIFACT_DOWNLOAD_PROGRESS_INTERVAL
    {
        return;
    }
    throttle.last_emit = Some(now);
    progress(&ArtifactInstallProgress::Downloading {
        transferred_bytes: update.transferred_bytes,
        total_bytes: update.total_bytes,
        active_requests: update.active_requests,
    });
}

/// One verified artifact download that must be atomically published to `target`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArtifactDownloadRequest {
    /// Stable identifier used in download diagnostics.
    pub task_id: String,
    /// Absolute primary artifact URL.
    pub url: String,
    /// Cache path that receives the verified artifact.
    pub target: PathBuf,
    /// Required SHA-1 digest.
    pub sha1: String,
    /// Optional required byte count.
    pub size: Option<u64>,
}

/// Downloads launcher metadata and verified artifacts.
///
/// `download` is retained as the bounded metadata/test-injection operation. Production
/// implementations override `download_many` and stream directly to each target.
pub trait ArtifactDownloader: Sync {
    /// Fetches one small metadata document such as a checksum sidecar.
    fn download(&self, url: &str) -> Result<Vec<u8>, ArtifactInstallError>;

    /// Publishes one batch under the downloader's shared request budget.
    fn download_many(
        &self,
        requests: &[ArtifactDownloadRequest],
    ) -> Vec<Result<(), ArtifactInstallError>> {
        requests
            .iter()
            .map(|request| {
                let bytes = self.download(&request.url)?;
                let verification = ArtifactVerification {
                    sha1: request.sha1.clone(),
                    size: request.size,
                };
                verify_bytes(&request.task_id, &bytes, &verification)?;
                atomic_write(&request.target, &bytes, "publish cache artifact")
            })
            .collect()
    }

    /// Publishes one batch while reporting byte progress, so a long artifact download keeps
    /// moving on screen instead of finishing silently.
    ///
    /// Implementations without a batch observer keep the plain behavior; the production
    /// downloader overrides this to stream the network engine's own progress events.
    fn download_many_observed(
        &self,
        requests: &[ArtifactDownloadRequest],
        _observer: Arc<ArtifactProgressObserver>,
    ) -> Vec<Result<(), ArtifactInstallError>> {
        self.download_many(requests)
    }
}

/// Callback receiving live byte progress for one artifact batch.
pub type ArtifactProgressObserver = dyn Fn(ArtifactDownloadProgress) + Send + Sync;

/// Byte progress for one running artifact batch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ArtifactDownloadProgress {
    /// Bytes received so far across the batch.
    pub transferred_bytes: u64,
    /// Expected bytes across the batch, as far as it is known.
    pub total_bytes: u64,
    /// Requests currently in flight across the shared engine.
    pub active_requests: usize,
}

impl<T: ArtifactDownloader + ?Sized> ArtifactDownloader for &T {
    fn download(&self, url: &str) -> Result<Vec<u8>, ArtifactInstallError> {
        (**self).download(url)
    }

    fn download_many(
        &self,
        requests: &[ArtifactDownloadRequest],
    ) -> Vec<Result<(), ArtifactInstallError>> {
        (**self).download_many(requests)
    }

    /// Forwarded so a borrowed downloader keeps reporting progress.
    ///
    /// Without this the blanket implementation would silently fall back to the plain batch and
    /// drop every byte update.
    fn download_many_observed(
        &self,
        requests: &[ArtifactDownloadRequest],
        observer: Arc<ArtifactProgressObserver>,
    ) -> Vec<Result<(), ArtifactInstallError>> {
        (**self).download_many_observed(requests, observer)
    }
}

/// Installs every file referenced by a resolved launch descriptor.
pub trait ArtifactInstaller {
    /// Verifies cached artifacts, downloads missing files, copies them into the run directory,
    /// and extracts native archives into `run/natives`.
    fn install(
        &self,
        descriptor: &LaunchDescriptor,
        run_dir: &Path,
        cache_dir: &Path,
    ) -> Result<(), ArtifactInstallError>;

    /// Installs artifacts while reporting cache and download progress.
    ///
    /// Implementations that cannot expose individual progress may retain the default behavior;
    /// the production installer overrides this method.
    fn install_with_progress(
        &self,
        descriptor: &LaunchDescriptor,
        run_dir: &Path,
        cache_dir: &Path,
        _progress: &mut dyn FnMut(&ArtifactInstallProgress),
    ) -> Result<(), ArtifactInstallError> {
        self.install(descriptor, run_dir, cache_dir)
    }
}

/// Filesystem implementation that uses a caller-provided downloader.
pub struct ArtifactInstallerImpl<D> {
    downloader: D,
}

impl<D> ArtifactInstallerImpl<D> {
    /// Creates an installer using the supplied transport implementation.
    pub fn new(downloader: D) -> Self {
        Self { downloader }
    }
}

impl<D: ArtifactDownloader> ArtifactInstaller for ArtifactInstallerImpl<D> {
    fn install(
        &self,
        descriptor: &LaunchDescriptor,
        run_dir: &Path,
        cache_dir: &Path,
    ) -> Result<(), ArtifactInstallError> {
        self.install_with_progress(descriptor, run_dir, cache_dir, &mut |_| {})
    }

    fn install_with_progress(
        &self,
        descriptor: &LaunchDescriptor,
        run_dir: &Path,
        cache_dir: &Path,
        progress: &mut dyn FnMut(&ArtifactInstallProgress),
    ) -> Result<(), ArtifactInstallError> {
        let mut total = descriptor.client_jar.iter().count()
            + descriptor.libraries.len()
            + descriptor.natives.len()
            + usize::from(descriptor.asset_index.is_some());
        let mut completed = 0;
        let mut throttle = DownloadProgressThrottle::default();
        progress(&ArtifactInstallProgress::Starting { total });
        let artifacts = descriptor
            .client_jar
            .iter()
            .chain(descriptor.libraries.iter())
            .chain(descriptor.natives.iter().map(|native| &native.artifact))
            .collect::<Vec<_>>();
        let installed = install_artifacts_with_progress(
            &self.downloader,
            cache_dir,
            run_dir,
            &artifacts,
            &mut |update| emit_download_progress(progress, &mut throttle, update),
        )?;
        let regular_count = descriptor.client_jar.iter().count() + descriptor.libraries.len();
        for (artifact, installed) in artifacts.iter().zip(installed.iter()).take(regular_count) {
            completed += 1;
            progress(&ArtifactInstallProgress::ArtifactReady {
                completed,
                total,
                path: artifact.path.clone(),
                source: installed.source,
            });
        }
        let mut installed_natives = Vec::with_capacity(descriptor.natives.len());
        for (native, installed) in descriptor
            .natives
            .iter()
            .zip(installed.iter().skip(regular_count))
        {
            completed += 1;
            progress(&ArtifactInstallProgress::ArtifactReady {
                completed,
                total,
                path: native.artifact.path.clone(),
                source: installed.source,
            });
            installed_natives.push((native, installed.clone()));
        }
        install_natives(&installed_natives, cache_dir, &run_dir.join("natives"))?;
        if let Some(asset_index) = &descriptor.asset_index {
            install_assets(
                &self.downloader,
                cache_dir,
                run_dir,
                asset_index,
                &mut completed,
                &mut total,
                progress,
            )?;
        }
        Ok(())
    }
}

/// Errors returned while caching, verifying, copying, or extracting launcher artifacts.
#[derive(Debug, Error)]
pub enum ArtifactInstallError {
    /// An artifact has no URL and is absent or invalid in the cache.
    #[error("artifact '{path}' is missing a download URL")]
    MissingUrl { path: String },
    /// A legacy Maven checksum sidecar could not supply a SHA-1 digest.
    #[error("artifact checksum sidecar '{url}' is invalid: {reason}")]
    InvalidChecksumSidecar { url: String, reason: String },
    /// An artifact path would escape its intended cache or run directory.
    #[error("artifact path '{path}' must be a safe relative path")]
    UnsafePath { path: String },
    /// A downloader failed to fetch an artifact.
    #[error("failed to download '{url}': {reason}")]
    Download { url: String, reason: String },
    /// A filesystem operation failed.
    #[error("failed to {operation} '{path}': {source}")]
    Io {
        operation: &'static str,
        path: String,
        #[source]
        source: std::io::Error,
    },
    /// A file differs from descriptor size metadata.
    #[error("artifact '{path}' has size {actual}, expected {expected}")]
    SizeMismatch {
        path: String,
        expected: u64,
        actual: u64,
    },
    /// A file differs from descriptor SHA-1 metadata.
    #[error("artifact '{path}' has SHA-1 {actual}, expected {expected}")]
    Sha1Mismatch {
        path: String,
        expected: String,
        actual: String,
    },
    /// A native archive cannot be read as ZIP data.
    #[error("failed to read native archive '{path}': {source}")]
    NativeArchive {
        path: String,
        #[source]
        source: zip::result::ZipError,
    },
    /// The asset index is not a valid Mojang asset-index document.
    #[error("failed to parse asset index '{path}': {source}")]
    AssetIndexJson {
        path: String,
        #[source]
        source: serde_json::Error,
    },
    /// An asset object has an invalid SHA-1 digest.
    #[error("asset object '{name}' has invalid SHA-1 digest '{hash}'")]
    InvalidAssetHash { name: String, hash: String },
    /// Two asset-index entries declare different sizes for the same content hash.
    #[error(
        "asset objects '{first_name}' and '{second_name}' share SHA-1 '{hash}' but declare sizes {first_size} and {second_size}"
    )]
    ConflictingAssetSize {
        first_name: String,
        second_name: String,
        hash: String,
        first_size: u64,
        second_size: u64,
    },
    /// Another mcmpu process retained the cache lock beyond the bounded wait.
    #[error("timed out waiting for runtime artifact cache lock '{path}'")]
    CacheLockTimeout { path: PathBuf },
    /// The operating system could not report enough information to size the artifact worker pool.
    #[error("failed to determine runtime artifact publication parallelism: {source}")]
    Parallelism {
        #[source]
        source: std::io::Error,
    },
    /// An artifact worker panicked instead of returning a structured failure.
    #[error("runtime artifact publication worker panicked")]
    WorkerPanicked,
    /// The internal ordered worker queue completed without returning one task result.
    #[error("runtime artifact publication did not return a result for task {index}")]
    MissingWorkerResult { index: usize },
}

#[derive(Clone)]
struct InstalledArtifact {
    file_path: PathBuf,
    source: ArtifactInstallSource,
    verification: ArtifactVerification,
}

fn install_artifact<D: ArtifactDownloader>(
    downloader: &D,
    cache_root: &Path,
    run_dir: &Path,
    artifact: &LaunchArtifact,
) -> Result<InstalledArtifact, ArtifactInstallError> {
    install_artifacts(downloader, cache_root, run_dir, &[artifact])?
        .into_iter()
        .next()
        .ok_or_else(|| ArtifactInstallError::Download {
            url: artifact.url.clone().unwrap_or_default(),
            reason: "single-artifact batch returned no result".to_string(),
        })
}

struct PendingArtifact<'a> {
    artifact: &'a LaunchArtifact,
    url: String,
    cached: PathBuf,
    verification: ArtifactVerification,
}

struct ResolvedArtifact<'a> {
    artifact: &'a LaunchArtifact,
    cached: PathBuf,
    target: PathBuf,
    verification: ArtifactVerification,
}

enum PreparedArtifact {
    Ready(InstalledArtifact),
    Download,
}

fn install_artifacts<D: ArtifactDownloader>(
    downloader: &D,
    cache_root: &Path,
    run_dir: &Path,
    artifacts: &[&LaunchArtifact],
) -> Result<Vec<InstalledArtifact>, ArtifactInstallError> {
    install_artifacts_with_progress(downloader, cache_root, run_dir, artifacts, &mut |_| {})
}

/// Installs artifacts while forwarding live byte progress for the download phase.
fn install_artifacts_with_progress<D: ArtifactDownloader>(
    downloader: &D,
    cache_root: &Path,
    run_dir: &Path,
    artifacts: &[&LaunchArtifact],
    on_download_progress: &mut dyn FnMut(ArtifactDownloadProgress),
) -> Result<Vec<InstalledArtifact>, ArtifactInstallError> {
    if artifacts.is_empty() {
        return Ok(Vec::new());
    }

    let mut resolved = Vec::with_capacity(artifacts.len());
    for artifact in artifacts {
        let relative = safe_relative(&artifact.path)?;
        let verification = resolve_verification(downloader, cache_root, artifact)?;
        let target = run_dir.join(&relative);
        let cached = cache_root.join(relative);
        resolved.push(ResolvedArtifact {
            artifact,
            cached,
            target,
            verification,
        });
    }

    let initial_target_hits = parallel_ordered(&resolved, |_, resolved| {
        valid_file(
            &resolved.target,
            &resolved.artifact.path,
            &resolved.verification,
            "read run artifact",
        )
    })?;

    let missing_target_count = initial_target_hits.iter().filter(|hit| !**hit).count();
    let mut lock_paths = if missing_target_count == 0 {
        Vec::new()
    } else {
        let lock_directory = ensure_artifact_lock_directory(cache_root)?;
        resolved
            .iter()
            .zip(&initial_target_hits)
            .filter(|(_, target_hit)| !**target_hit)
            .map(|(resolved, _)| artifact_lock_path_in(&lock_directory, &resolved.artifact.path))
            .collect()
    };

    lock_paths.sort();
    lock_paths.dedup();
    let mut locks = Vec::with_capacity(lock_paths.len());
    for path in lock_paths {
        locks.push(ArtifactCacheLock::acquire(&path)?);
    }

    let prepared = parallel_ordered(&resolved, |index, resolved| {
        if initial_target_hits[index]
            || valid_file(
                &resolved.target,
                &resolved.artifact.path,
                &resolved.verification,
                "read run artifact after cache lock",
            )?
        {
            return Ok(PreparedArtifact::Ready(InstalledArtifact {
                file_path: resolved.target.clone(),
                source: ArtifactInstallSource::TargetHit,
                verification: resolved.verification.clone(),
            }));
        }

        match atomic_copy_verified(
            &resolved.cached,
            &resolved.target,
            &resolved.artifact.path,
            &resolved.verification,
            "publish cached run artifact",
        )? {
            VerifiedCopyOutcome::Published => Ok(PreparedArtifact::Ready(InstalledArtifact {
                file_path: resolved.target.clone(),
                source: ArtifactInstallSource::CacheHit,
                verification: resolved.verification.clone(),
            })),
            VerifiedCopyOutcome::SourceMissing | VerifiedCopyOutcome::SourceInvalid(_) => {
                Ok(PreparedArtifact::Download)
            }
        }
    })?;

    let mut pending = Vec::new();
    for (resolved, prepared) in resolved.iter().zip(&prepared) {
        if !matches!(prepared, PreparedArtifact::Download) {
            continue;
        }
        let url = resolved
            .artifact
            .url
            .as_deref()
            .filter(|url| !url.trim().is_empty())
            .ok_or_else(|| ArtifactInstallError::MissingUrl {
                path: resolved.artifact.path.clone(),
            })?;
        pending.push(PendingArtifact {
            artifact: resolved.artifact,
            url: url.to_string(),
            cached: resolved.cached.clone(),
            verification: resolved.verification.clone(),
        });
    }

    let requests = pending
        .iter()
        .map(|pending| ArtifactDownloadRequest {
            task_id: pending.artifact.path.clone(),
            url: pending.url.clone(),
            target: pending.cached.clone(),
            sha1: pending.verification.sha1.clone(),
            size: pending.verification.size,
        })
        .collect::<Vec<_>>();
    let results = if requests.is_empty() {
        Vec::new()
    } else {
        // A long batch stays visible: the download runs on a worker while this thread forwards
        // each byte snapshot to the caller. The caller's callback never leaves its own thread,
        // and a transfer worker never blocks on a full queue.
        let (sender, receiver) = mpsc::sync_channel::<ArtifactDownloadProgress>(64);
        let observer: Arc<ArtifactProgressObserver> = Arc::new(move |update| {
            let _ = sender.try_send(update);
        });
        thread::scope(|scope| {
            let worker = scope.spawn(|| downloader.download_many_observed(&requests, observer));
            while !worker.is_finished() {
                match receiver.recv_timeout(Duration::from_millis(50)) {
                    Ok(update) => on_download_progress(update),
                    Err(mpsc::RecvTimeoutError::Timeout) => {}
                    Err(mpsc::RecvTimeoutError::Disconnected) => break,
                }
            }
            while let Ok(update) = receiver.try_recv() {
                on_download_progress(update);
            }
            worker.join().expect("artifact download worker panicked")
        })
    };
    if results.len() != requests.len() {
        return Err(ArtifactInstallError::Download {
            url: "artifact batch".to_string(),
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

    let downloaded = parallel_ordered(&pending, |_, pending| {
        let target = run_dir.join(safe_relative(&pending.artifact.path)?);
        match atomic_copy_verified(
            &pending.cached,
            &target,
            &pending.artifact.path,
            &pending.verification,
            "publish downloaded run artifact",
        )? {
            VerifiedCopyOutcome::Published => Ok(InstalledArtifact {
                file_path: target,
                source: ArtifactInstallSource::Downloaded,
                verification: pending.verification.clone(),
            }),
            VerifiedCopyOutcome::SourceMissing => Err(ArtifactInstallError::Io {
                operation: "open downloaded cache artifact",
                path: pending.cached.display().to_string(),
                source: std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    "downloader reported success without publishing the cache artifact",
                ),
            }),
            VerifiedCopyOutcome::SourceInvalid(error) => Err(error),
        }
    })?;

    let mut downloaded = downloaded.into_iter();
    let mut installed = Vec::with_capacity(resolved.len());
    for prepared in prepared {
        match prepared {
            PreparedArtifact::Ready(artifact) => installed.push(artifact),
            PreparedArtifact::Download => installed.push(downloaded.next().ok_or(
                ArtifactInstallError::MissingWorkerResult {
                    index: installed.len(),
                },
            )?),
        }
    }
    drop(locks);
    Ok(installed)
}

fn parallel_ordered<T, R, F>(items: &[T], operation: F) -> Result<Vec<R>, ArtifactInstallError>
where
    T: Sync,
    R: Send,
    F: Fn(usize, &T) -> Result<R, ArtifactInstallError> + Sync,
{
    if items.is_empty() {
        return Ok(Vec::new());
    }
    let workers = artifact_worker_count(items.len())?;
    let next = AtomicUsize::new(0);
    let cancelled = AtomicBool::new(false);
    let (sender, receiver) = mpsc::channel();

    let workers_completed = thread::scope(|scope| {
        let mut handles = Vec::with_capacity(workers);
        for _ in 0..workers {
            let sender = sender.clone();
            let operation = &operation;
            let next = &next;
            let cancelled = &cancelled;
            handles.push(scope.spawn(move || {
                loop {
                    if cancelled.load(Ordering::Acquire) {
                        break;
                    }
                    let index = next.fetch_add(1, Ordering::Relaxed);
                    if index >= items.len() {
                        break;
                    }
                    let result = operation(index, &items[index]);
                    if result.is_err() {
                        cancelled.store(true, Ordering::Release);
                    }
                    if sender.send((index, result)).is_err() {
                        break;
                    }
                }
            }));
        }
        for handle in handles {
            if handle.join().is_err() {
                return Err(ArtifactInstallError::WorkerPanicked);
            }
        }
        Ok(())
    });
    drop(sender);
    workers_completed?;

    let mut results = std::iter::repeat_with(|| None)
        .take(items.len())
        .collect::<Vec<_>>();
    for (index, result) in receiver {
        results[index] = Some(result);
    }

    let mut completed = Vec::with_capacity(items.len());
    for (index, result) in results.into_iter().enumerate() {
        match result {
            Some(Ok(value)) => completed.push(value),
            Some(Err(error)) => return Err(error),
            None => return Err(ArtifactInstallError::MissingWorkerResult { index }),
        }
    }
    Ok(completed)
}

fn artifact_worker_count(jobs: usize) -> Result<usize, ArtifactInstallError> {
    let parallelism = thread::available_parallelism()
        .map_err(|source| ArtifactInstallError::Parallelism { source })?
        .get();
    Ok(parallelism
        .saturating_mul(2)
        .clamp(MIN_ARTIFACT_WORKERS, MAX_ARTIFACT_WORKERS)
        .min(jobs))
}

const NATIVES_MARKER_NAME: &str = ".mcmpu-natives.json";

#[derive(Debug, Serialize, Deserialize)]
struct NativesMarker {
    format: u32,
    descriptor_sha1: String,
    files: BTreeMap<String, NativeOutput>,
}

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
struct NativeOutput {
    sha1: String,
    size: u64,
}

fn install_natives(
    natives: &[(&NativeArtifact, InstalledArtifact)],
    cache_root: &Path,
    natives_dir: &Path,
) -> Result<(), ArtifactInstallError> {
    if natives.is_empty() {
        return Ok(());
    }
    let descriptor_sha1 = native_descriptor_sha1(natives);
    let lock_key = format!("natives:{}:{descriptor_sha1}", natives_dir.display());
    let _lock = ArtifactCacheLock::acquire(&artifact_lock_path(cache_root, &lock_key)?)?;
    if valid_natives_marker(natives_dir, &descriptor_sha1)? {
        return Ok(());
    }

    let staging = unique_part_path(natives_dir);
    remove_path_if_exists(&staging, "remove stale native staging directory")?;
    fs::create_dir_all(&staging).map_err(|source| ArtifactInstallError::Io {
        operation: "create native staging directory",
        path: staging.display().to_string(),
        source,
    })?;

    let result = (|| {
        for (native, installed) in natives {
            extract_native_archive(&installed.file_path, &staging, native)?;
        }
        let marker = NativesMarker {
            format: 1,
            descriptor_sha1,
            files: collect_native_outputs(&staging)?,
        };
        let marker_bytes =
            serde_json::to_vec_pretty(&marker).map_err(|source| ArtifactInstallError::Io {
                operation: "serialize native marker",
                path: staging.join(NATIVES_MARKER_NAME).display().to_string(),
                source: std::io::Error::other(source),
            })?;
        atomic_write(
            &staging.join(NATIVES_MARKER_NAME),
            &marker_bytes,
            "publish native marker",
        )?;
        replace_directory(&staging, natives_dir)
    })();
    match result {
        Ok(()) => Ok(()),
        Err(error) => Err(cleanup_directory_after_error(&staging, error)),
    }
}

fn artifact_lock_path(cache_root: &Path, key: &str) -> Result<PathBuf, ArtifactInstallError> {
    let locks = ensure_artifact_lock_directory(cache_root)?;
    Ok(artifact_lock_path_in(&locks, key))
}

fn ensure_artifact_lock_directory(cache_root: &Path) -> Result<PathBuf, ArtifactInstallError> {
    let locks = cache_root.join(".locks");
    fs::create_dir_all(&locks).map_err(|source| ArtifactInstallError::Io {
        operation: "create runtime artifact lock directory",
        path: locks.display().to_string(),
        source,
    })?;
    Ok(locks)
}

fn artifact_lock_path_in(lock_directory: &Path, key: &str) -> PathBuf {
    let digest = format!("{:x}", Sha1::digest(key.as_bytes()));
    lock_directory.join(format!("{digest}.lock"))
}

struct ArtifactCacheLock {
    path: PathBuf,
    file: fs::File,
}

impl ArtifactCacheLock {
    fn acquire(path: &Path) -> Result<Self, ArtifactInstallError> {
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)
            .map_err(|source| ArtifactInstallError::Io {
                operation: "open runtime artifact cache lock",
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
                        .map_err(|source| ArtifactInstallError::Io {
                            operation: "initialize runtime artifact cache lock",
                            path: path.display().to_string(),
                            source,
                        })?;
                    return Ok(Self {
                        path: path.to_path_buf(),
                        file,
                    });
                }
                Err(std::fs::TryLockError::WouldBlock)
                    if started.elapsed() < ARTIFACT_CACHE_LOCK_TIMEOUT =>
                {
                    thread::sleep(Duration::from_millis(100));
                }
                Err(std::fs::TryLockError::WouldBlock) => {
                    return Err(ArtifactInstallError::CacheLockTimeout {
                        path: path.to_path_buf(),
                    });
                }
                Err(std::fs::TryLockError::Error(source)) => {
                    return Err(ArtifactInstallError::Io {
                        operation: "acquire runtime artifact cache lock",
                        path: path.display().to_string(),
                        source,
                    });
                }
            }
        }
    }
}

impl Drop for ArtifactCacheLock {
    fn drop(&mut self) {
        if let Err(error) = self.file.unlock() {
            eprintln!(
                "failed to release runtime artifact cache lock '{}': {error}",
                self.path.display()
            );
        }
    }
}

fn native_descriptor_sha1(natives: &[(&NativeArtifact, InstalledArtifact)]) -> String {
    let mut hasher = Sha1::new();
    hasher.update(b"mcmpu-natives-v1\0");
    for (native, installed) in natives {
        for value in [
            native.library_name.as_str(),
            native.classifier.as_str(),
            native.artifact.path.as_str(),
            installed.verification.sha1.as_str(),
        ] {
            hasher.update((value.len() as u64).to_le_bytes());
            hasher.update(value.as_bytes());
        }
        hasher.update(
            installed
                .verification
                .size
                .unwrap_or(u64::MAX)
                .to_le_bytes(),
        );
        hasher.update((native.extract_excludes.len() as u64).to_le_bytes());
        for exclude in &native.extract_excludes {
            hasher.update((exclude.len() as u64).to_le_bytes());
            hasher.update(exclude.as_bytes());
        }
    }
    format!("{:x}", hasher.finalize())
}

fn valid_natives_marker(
    natives_dir: &Path,
    expected_descriptor_sha1: &str,
) -> Result<bool, ArtifactInstallError> {
    let marker_path = natives_dir.join(NATIVES_MARKER_NAME);
    let bytes = match fs::read(&marker_path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(source) => {
            return Err(ArtifactInstallError::Io {
                operation: "read native marker",
                path: marker_path.display().to_string(),
                source,
            });
        }
    };
    let marker: NativesMarker = match serde_json::from_slice(&bytes) {
        Ok(marker) => marker,
        Err(_) => return Ok(false),
    };
    if marker.format != 1 || marker.descriptor_sha1 != expected_descriptor_sha1 {
        return Ok(false);
    }
    Ok(collect_native_outputs(natives_dir)? == marker.files)
}

fn extract_native_archive(
    archive_path: &Path,
    natives_dir: &Path,
    native: &NativeArtifact,
) -> Result<(), ArtifactInstallError> {
    let file = fs::File::open(archive_path).map_err(|source| ArtifactInstallError::Io {
        operation: "open native archive",
        path: archive_path.display().to_string(),
        source,
    })?;
    let mut archive =
        zip::ZipArchive::new(file).map_err(|source| ArtifactInstallError::NativeArchive {
            path: archive_path.display().to_string(),
            source,
        })?;
    for index in 0..archive.len() {
        let mut entry =
            archive
                .by_index(index)
                .map_err(|source| ArtifactInstallError::NativeArchive {
                    path: archive_path.display().to_string(),
                    source,
                })?;
        let name = entry.name().replace('\\', "/");
        if entry.is_dir()
            || native
                .extract_excludes
                .iter()
                .any(|prefix| name.starts_with(prefix))
        {
            continue;
        }
        let relative = safe_relative(&name)?;
        let target = natives_dir.join(relative);
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent).map_err(|source| ArtifactInstallError::Io {
                operation: "create native directory for",
                path: parent.display().to_string(),
                source,
            })?;
        }
        let mut output = fs::File::create(&target).map_err(|source| ArtifactInstallError::Io {
            operation: "create extracted native",
            path: target.display().to_string(),
            source,
        })?;
        std::io::copy(&mut entry, &mut output).map_err(|source| ArtifactInstallError::Io {
            operation: "extract native",
            path: target.display().to_string(),
            source,
        })?;
        output.flush().map_err(|source| ArtifactInstallError::Io {
            operation: "flush extracted native",
            path: target.display().to_string(),
            source,
        })?;
        output
            .sync_all()
            .map_err(|source| ArtifactInstallError::Io {
                operation: "sync extracted native",
                path: target.display().to_string(),
                source,
            })?;
    }
    Ok(())
}

fn collect_native_outputs(
    root: &Path,
) -> Result<BTreeMap<String, NativeOutput>, ArtifactInstallError> {
    let mut pending = vec![root.to_path_buf()];
    let mut outputs = BTreeMap::new();
    while let Some(directory) = pending.pop() {
        let entries = fs::read_dir(&directory).map_err(|source| ArtifactInstallError::Io {
            operation: "read native staging directory",
            path: directory.display().to_string(),
            source,
        })?;
        for entry in entries {
            let entry = entry.map_err(|source| ArtifactInstallError::Io {
                operation: "read native staging entry",
                path: directory.display().to_string(),
                source,
            })?;
            let file_type = entry
                .file_type()
                .map_err(|source| ArtifactInstallError::Io {
                    operation: "inspect native staging entry",
                    path: entry.path().display().to_string(),
                    source,
                })?;
            if file_type.is_dir() {
                pending.push(entry.path());
            } else if file_type.is_file() {
                let entry_path = entry.path();
                let relative =
                    entry_path
                        .strip_prefix(root)
                        .map_err(|source| ArtifactInstallError::Io {
                            operation: "resolve native output path",
                            path: entry_path.display().to_string(),
                            source: std::io::Error::other(source),
                        })?;
                if relative == Path::new(NATIVES_MARKER_NAME) {
                    continue;
                }
                let bytes = fs::read(&entry_path).map_err(|source| ArtifactInstallError::Io {
                    operation: "read extracted native",
                    path: entry_path.display().to_string(),
                    source,
                })?;
                outputs.insert(
                    portable_relative(relative),
                    NativeOutput {
                        sha1: format!("{:x}", Sha1::digest(&bytes)),
                        size: bytes.len() as u64,
                    },
                );
            }
        }
    }
    Ok(outputs)
}

fn install_assets<D: ArtifactDownloader>(
    downloader: &D,
    cache_root: &Path,
    run_dir: &Path,
    metadata: &crate::project::launcher::AssetIndexMetadata,
    completed: &mut usize,
    total: &mut usize,
    progress: &mut dyn FnMut(&ArtifactInstallProgress),
) -> Result<(), ArtifactInstallError> {
    let index_artifact = LaunchArtifact {
        path: format!("assets/indexes/{}.json", metadata.id),
        url: Some(metadata.url.clone()),
        sha1: Some(metadata.sha1.clone()),
        size: Some(metadata.size),
    };
    let index_artifact_installed =
        install_artifact(downloader, cache_root, run_dir, &index_artifact)?;
    let index_text = fs::read_to_string(&index_artifact_installed.file_path).map_err(|source| {
        ArtifactInstallError::Io {
            operation: "read asset index",
            path: index_artifact_installed.file_path.display().to_string(),
            source,
        }
    })?;
    let index: AssetIndexDocument = serde_json::from_str(&index_text).map_err(|source| {
        ArtifactInstallError::AssetIndexJson {
            path: index_artifact_installed.file_path.display().to_string(),
            source,
        }
    })?;
    let mut unique_objects = BTreeMap::<String, (String, AssetObject)>::new();
    for (name, object) in index.objects {
        validate_asset_hash(&name, &object.hash)?;
        let normalized_hash = object.hash.to_ascii_lowercase();
        match unique_objects.entry(normalized_hash.clone()) {
            std::collections::btree_map::Entry::Vacant(entry) => {
                entry.insert((name, object));
            }
            std::collections::btree_map::Entry::Occupied(entry) => {
                let (first_name, first_object) = entry.get();
                if first_object.size != object.size {
                    return Err(ArtifactInstallError::ConflictingAssetSize {
                        first_name: first_name.clone(),
                        second_name: name,
                        hash: normalized_hash,
                        first_size: first_object.size,
                        second_size: object.size,
                    });
                }
            }
        }
    }
    let artifacts: Vec<_> = unique_objects
        .into_iter()
        .map(|(hash, (_name, object))| LaunchArtifact {
            path: format!("assets/objects/{}/{}", &hash[..2], hash),
            url: Some(format!(
                "https://resources.download.minecraft.net/{}/{}",
                &hash[..2],
                hash
            )),
            sha1: Some(hash),
            size: Some(object.size),
        })
        .collect();
    *total += artifacts.len();
    progress(&ArtifactInstallProgress::TotalUpdated { total: *total });
    *completed += 1;
    progress(&ArtifactInstallProgress::ArtifactReady {
        completed: *completed,
        total: *total,
        path: index_artifact.path,
        source: index_artifact_installed.source,
    });
    if artifacts.is_empty() {
        return Ok(());
    }
    let artifact_refs = artifacts.iter().collect::<Vec<_>>();
    let installed = install_artifacts(downloader, cache_root, run_dir, &artifact_refs)?;
    for (artifact, installed) in artifacts.iter().zip(installed) {
        *completed += 1;
        progress(&ArtifactInstallProgress::ArtifactReady {
            completed: *completed,
            total: *total,
            path: artifact.path.clone(),
            source: installed.source,
        });
    }
    Ok(())
}

fn validate_asset_hash(name: &str, hash: &str) -> Result<(), ArtifactInstallError> {
    if hash.len() == 40 && hash.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Ok(());
    }
    Err(ArtifactInstallError::InvalidAssetHash {
        name: name.to_string(),
        hash: hash.to_string(),
    })
}

#[derive(Debug, Deserialize)]
struct AssetIndexDocument {
    objects: std::collections::BTreeMap<String, AssetObject>,
}

#[derive(Debug, Deserialize)]
struct AssetObject {
    hash: String,
    size: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ArtifactVerification {
    sha1: String,
    size: Option<u64>,
}

fn resolve_verification<D: ArtifactDownloader>(
    downloader: &D,
    cache_root: &Path,
    artifact: &LaunchArtifact,
) -> Result<ArtifactVerification, ArtifactInstallError> {
    if let Some(sha1) = artifact.sha1.as_deref() {
        return Ok(ArtifactVerification {
            sha1: normalize_sha1(sha1, &artifact.path, None)?,
            size: artifact.size,
        });
    }

    let url = artifact
        .url
        .as_deref()
        .filter(|url| !url.trim().is_empty())
        .ok_or_else(|| ArtifactInstallError::MissingUrl {
            path: artifact.path.clone(),
        })?;
    let checksum_url = format!("{url}.sha1");
    let checksum_cache = checksum_cache_path(cache_root, &checksum_url);
    if let Some(digest) = read_cached_checksum(&checksum_cache, &checksum_url, &artifact.path)? {
        return Ok(ArtifactVerification {
            sha1: digest,
            size: artifact.size,
        });
    }
    let bytes = downloader.download(&checksum_url)?;
    let text = std::str::from_utf8(&bytes).map_err(|error| {
        ArtifactInstallError::InvalidChecksumSidecar {
            url: checksum_url.clone(),
            reason: error.to_string(),
        }
    })?;
    let digest = text.split_whitespace().next().ok_or_else(|| {
        ArtifactInstallError::InvalidChecksumSidecar {
            url: checksum_url.clone(),
            reason: "checksum sidecar is empty".to_string(),
        }
    })?;
    let sha1 = normalize_sha1(digest, &artifact.path, Some(&checksum_url))?;
    atomic_write(
        &checksum_cache,
        format!("{sha1}\n").as_bytes(),
        "publish checksum sidecar cache",
    )?;
    Ok(ArtifactVerification {
        sha1,
        size: artifact.size,
    })
}

fn checksum_cache_path(cache_root: &Path, checksum_url: &str) -> PathBuf {
    let key = format!("{:x}", Sha1::digest(checksum_url.as_bytes()));
    cache_root.join(".checksums").join(format!("{key}.sha1"))
}

fn read_cached_checksum(
    path: &Path,
    checksum_url: &str,
    artifact_path: &str,
) -> Result<Option<String>, ArtifactInstallError> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(source) => {
            return Err(ArtifactInstallError::Io {
                operation: "read cached checksum sidecar",
                path: path.display().to_string(),
                source,
            });
        }
    };
    let text = match std::str::from_utf8(&bytes) {
        Ok(text) => text,
        Err(_) => return Ok(None),
    };
    let Some(digest) = text.split_whitespace().next() else {
        return Ok(None);
    };
    match normalize_sha1(digest, artifact_path, Some(checksum_url)) {
        Ok(digest) => Ok(Some(digest)),
        Err(_) => Ok(None),
    }
}

fn normalize_sha1(
    value: &str,
    artifact_path: &str,
    checksum_url: Option<&str>,
) -> Result<String, ArtifactInstallError> {
    if value.len() == 40 && value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Ok(value.to_ascii_lowercase());
    }
    match checksum_url {
        Some(url) => Err(ArtifactInstallError::InvalidChecksumSidecar {
            url: url.to_string(),
            reason: format!("expected a 40-character SHA-1 digest for '{artifact_path}'"),
        }),
        None => Err(ArtifactInstallError::InvalidChecksumSidecar {
            url: "version metadata".to_string(),
            reason: format!("artifact '{artifact_path}' declares an invalid SHA-1 digest"),
        }),
    }
}

fn valid_file(
    path: &Path,
    artifact_path: &str,
    verification: &ArtifactVerification,
    read_operation: &'static str,
) -> Result<bool, ArtifactInstallError> {
    let mut file = match fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(source) => {
            return Err(ArtifactInstallError::Io {
                operation: read_operation,
                path: path.display().to_string(),
                source,
            });
        }
    };
    if let Some(expected) = verification.size {
        let actual = file
            .metadata()
            .map_err(|source| ArtifactInstallError::Io {
                operation: read_operation,
                path: path.display().to_string(),
                source,
            })?
            .len();
        if actual != expected {
            return Ok(false);
        }
    }
    let digest = sha1_reader(&mut file, path, read_operation)?;
    let actual = format!("{:x}", digest.finalize());
    let _ = artifact_path;
    Ok(actual.eq_ignore_ascii_case(&verification.sha1))
}

fn sha1_reader(
    reader: &mut impl Read,
    path: &Path,
    operation: &'static str,
) -> Result<Sha1, ArtifactInstallError> {
    let mut digest = Sha1::new();
    let mut buffer = [0_u8; 256 * 1024];
    loop {
        let read = reader
            .read(&mut buffer)
            .map_err(|source| ArtifactInstallError::Io {
                operation,
                path: path.display().to_string(),
                source,
            })?;
        if read == 0 {
            break;
        }
        digest.update(&buffer[..read]);
    }
    Ok(digest)
}

static PART_COUNTER: AtomicU64 = AtomicU64::new(0);

fn atomic_write(
    destination: &Path,
    bytes: &[u8],
    operation: &'static str,
) -> Result<(), ArtifactInstallError> {
    ensure_parent(destination)?;
    let temporary = unique_part_path(destination);
    let result = (|| {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
            .map_err(|source| ArtifactInstallError::Io {
                operation: "create temporary artifact",
                path: temporary.display().to_string(),
                source,
            })?;
        file.write_all(bytes)
            .map_err(|source| ArtifactInstallError::Io {
                operation: "write temporary artifact",
                path: temporary.display().to_string(),
                source,
            })?;
        file.flush().map_err(|source| ArtifactInstallError::Io {
            operation: "flush temporary artifact",
            path: temporary.display().to_string(),
            source,
        })?;
        drop(file);
        atomic_replace_file(&temporary, destination, operation)
    })();
    match result {
        Ok(()) => Ok(()),
        Err(error) => Err(cleanup_file_after_error(&temporary, error)),
    }
}

enum VerifiedCopyOutcome {
    Published,
    SourceMissing,
    SourceInvalid(ArtifactInstallError),
}

fn atomic_copy_verified(
    source: &Path,
    destination: &Path,
    artifact_path: &str,
    verification: &ArtifactVerification,
    operation: &'static str,
) -> Result<VerifiedCopyOutcome, ArtifactInstallError> {
    let mut input = match fs::File::open(source) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(VerifiedCopyOutcome::SourceMissing);
        }
        Err(source_error) => {
            return Err(ArtifactInstallError::Io {
                operation: "open artifact for verified copy",
                path: source.display().to_string(),
                source: source_error,
            });
        }
    };
    if let Some(expected) = verification.size {
        let actual = input
            .metadata()
            .map_err(|source_error| ArtifactInstallError::Io {
                operation: "inspect artifact for verified copy",
                path: source.display().to_string(),
                source: source_error,
            })?
            .len();
        if actual != expected {
            return Ok(VerifiedCopyOutcome::SourceInvalid(
                ArtifactInstallError::SizeMismatch {
                    path: artifact_path.to_string(),
                    expected,
                    actual,
                },
            ));
        }
    }

    ensure_parent(destination)?;
    let temporary = unique_part_path(destination);
    let result = (|| {
        let mut output = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
            .map_err(|source_error| ArtifactInstallError::Io {
                operation: "create temporary artifact copy",
                path: temporary.display().to_string(),
                source: source_error,
            })?;
        let mut digest = Sha1::new();
        let mut copied = 0_u64;
        let mut buffer = [0_u8; 256 * 1024];
        loop {
            let read =
                input
                    .read(&mut buffer)
                    .map_err(|source_error| ArtifactInstallError::Io {
                        operation: "read artifact for verified copy",
                        path: source.display().to_string(),
                        source: source_error,
                    })?;
            if read == 0 {
                break;
            }
            output
                .write_all(&buffer[..read])
                .map_err(|source_error| ArtifactInstallError::Io {
                    operation: "copy artifact into temporary file",
                    path: temporary.display().to_string(),
                    source: source_error,
                })?;
            digest.update(&buffer[..read]);
            copied += read as u64;
        }
        output
            .flush()
            .map_err(|source_error| ArtifactInstallError::Io {
                operation: "flush temporary artifact copy",
                path: temporary.display().to_string(),
                source: source_error,
            })?;
        drop(output);
        if let Some(expected) = verification.size
            && copied != expected
        {
            return Ok(VerifiedCopyOutcome::SourceInvalid(
                ArtifactInstallError::SizeMismatch {
                    path: artifact_path.to_string(),
                    expected,
                    actual: copied,
                },
            ));
        }
        let actual = format!("{:x}", digest.finalize());
        if !actual.eq_ignore_ascii_case(&verification.sha1) {
            return Ok(VerifiedCopyOutcome::SourceInvalid(
                ArtifactInstallError::Sha1Mismatch {
                    path: artifact_path.to_string(),
                    expected: verification.sha1.clone(),
                    actual,
                },
            ));
        }
        atomic_replace_file(&temporary, destination, operation)?;
        Ok(VerifiedCopyOutcome::Published)
    })();
    match result {
        Ok(VerifiedCopyOutcome::Published) => Ok(VerifiedCopyOutcome::Published),
        Ok(invalid @ VerifiedCopyOutcome::SourceInvalid(_)) => match fs::remove_file(&temporary) {
            Ok(()) => Ok(invalid),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(invalid),
            Err(source_error) => Err(ArtifactInstallError::Io {
                operation: "remove invalid temporary artifact copy",
                path: temporary.display().to_string(),
                source: source_error,
            }),
        },
        Ok(VerifiedCopyOutcome::SourceMissing) => Err(ArtifactInstallError::Io {
            operation: "complete verified artifact copy",
            path: source.display().to_string(),
            source: std::io::Error::other(
                "verified copy entered an invalid source-missing state after opening the source",
            ),
        }),
        Err(error) => Err(cleanup_file_after_error(&temporary, error)),
    }
}

fn ensure_parent(path: &Path) -> Result<(), ArtifactInstallError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|source| ArtifactInstallError::Io {
            operation: "create artifact parent directory",
            path: parent.display().to_string(),
            source,
        })?;
    }
    Ok(())
}

fn unique_part_path(destination: &Path) -> PathBuf {
    let counter = PART_COUNTER.fetch_add(1, Ordering::Relaxed);
    let name = destination
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("artifact");
    destination.with_file_name(format!(".{name}.{}.{}.part", std::process::id(), counter))
}

#[cfg(not(windows))]
fn atomic_replace_file(
    temporary: &Path,
    destination: &Path,
    operation: &'static str,
) -> Result<(), ArtifactInstallError> {
    fs::rename(temporary, destination).map_err(|source| ArtifactInstallError::Io {
        operation,
        path: destination.display().to_string(),
        source,
    })
}

#[cfg(windows)]
fn atomic_replace_file(
    temporary: &Path,
    destination: &Path,
    operation: &'static str,
) -> Result<(), ArtifactInstallError> {
    use std::os::windows::ffi::OsStrExt;

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn MoveFileExW(existing: *const u16, replacement: *const u16, flags: u32) -> i32;
    }
    const MOVEFILE_REPLACE_EXISTING: u32 = 0x1;
    let existing: Vec<u16> = temporary.as_os_str().encode_wide().chain(Some(0)).collect();
    let replacement: Vec<u16> = destination
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect();
    let moved = unsafe {
        MoveFileExW(
            existing.as_ptr(),
            replacement.as_ptr(),
            MOVEFILE_REPLACE_EXISTING,
        )
    };
    if moved == 0 {
        return Err(ArtifactInstallError::Io {
            operation,
            path: destination.display().to_string(),
            source: std::io::Error::last_os_error(),
        });
    }
    Ok(())
}

fn replace_directory(staging: &Path, destination: &Path) -> Result<(), ArtifactInstallError> {
    let backup = unique_part_path(destination).with_extension("old");
    let destination_exists = destination.exists();
    if destination_exists {
        fs::rename(destination, &backup).map_err(|source| ArtifactInstallError::Io {
            operation: "move previous native directory aside",
            path: destination.display().to_string(),
            source,
        })?;
    }
    if let Err(source) = fs::rename(staging, destination) {
        if destination_exists && let Err(restore_error) = fs::rename(&backup, destination) {
            return Err(ArtifactInstallError::Io {
                operation: "restore previous native directory after publish failure",
                path: destination.display().to_string(),
                source: std::io::Error::other(format!(
                    "publish failed: {source}; restore failed: {restore_error}"
                )),
            });
        }
        return Err(ArtifactInstallError::Io {
            operation: "publish native directory",
            path: destination.display().to_string(),
            source,
        });
    }
    if destination_exists {
        fs::remove_dir_all(&backup).map_err(|source| ArtifactInstallError::Io {
            operation: "remove previous native directory",
            path: backup.display().to_string(),
            source,
        })?;
    }
    Ok(())
}

fn cleanup_file_after_error(path: &Path, primary: ArtifactInstallError) -> ArtifactInstallError {
    match fs::remove_file(path) {
        Ok(()) => primary,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => primary,
        Err(cleanup) => ArtifactInstallError::Io {
            operation: "clean temporary artifact after failure",
            path: path.display().to_string(),
            source: std::io::Error::other(format!(
                "original failure: {primary}; cleanup failure: {cleanup}"
            )),
        },
    }
}

fn cleanup_directory_after_error(
    path: &Path,
    primary: ArtifactInstallError,
) -> ArtifactInstallError {
    match fs::remove_dir_all(path) {
        Ok(()) => primary,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => primary,
        Err(cleanup) => ArtifactInstallError::Io {
            operation: "clean native staging directory after failure",
            path: path.display().to_string(),
            source: std::io::Error::other(format!(
                "original failure: {primary}; cleanup failure: {cleanup}"
            )),
        },
    }
}

fn remove_path_if_exists(path: &Path, operation: &'static str) -> Result<(), ArtifactInstallError> {
    match fs::remove_dir_all(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(ArtifactInstallError::Io {
            operation,
            path: path.display().to_string(),
            source,
        }),
    }
}

fn portable_relative(path: &Path) -> String {
    path.components()
        .filter_map(|component| match component {
            Component::Normal(value) => Some(value.to_string_lossy().into_owned()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("/")
}

fn verify_bytes(
    path: &str,
    bytes: &[u8],
    verification: &ArtifactVerification,
) -> Result<(), ArtifactInstallError> {
    let actual_size = bytes.len() as u64;
    if let Some(expected_size) = verification.size
        && actual_size != expected_size
    {
        return Err(ArtifactInstallError::SizeMismatch {
            path: path.to_string(),
            expected: expected_size,
            actual: actual_size,
        });
    }
    let actual_sha1 = format!("{:x}", Sha1::digest(bytes));
    if !actual_sha1.eq_ignore_ascii_case(&verification.sha1) {
        return Err(ArtifactInstallError::Sha1Mismatch {
            path: path.to_string(),
            expected: verification.sha1.clone(),
            actual: actual_sha1,
        });
    }
    Ok(())
}

fn safe_relative(raw: &str) -> Result<PathBuf, ArtifactInstallError> {
    let path = Path::new(raw);
    if raw.trim().is_empty()
        || path.is_absolute()
        || !path
            .components()
            .all(|part| matches!(part, Component::Normal(_)))
    {
        return Err(ArtifactInstallError::UnsafePath {
            path: raw.to_string(),
        });
    }
    Ok(path.to_path_buf())
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use super::*;

    #[test]
    fn ordered_workers_execute_concurrently_and_preserve_input_order() {
        let jobs = (0..64).collect::<Vec<_>>();
        let workers = artifact_worker_count(jobs.len()).unwrap();
        let barrier = Arc::new(std::sync::Barrier::new(workers));
        let active = AtomicUsize::new(0);
        let peak = AtomicUsize::new(0);

        let results = parallel_ordered(&jobs, |_, value| {
            let current = active.fetch_add(1, Ordering::SeqCst) + 1;
            peak.fetch_max(current, Ordering::SeqCst);
            if *value < workers {
                barrier.wait();
            }
            std::thread::sleep(Duration::from_millis(2));
            active.fetch_sub(1, Ordering::SeqCst);
            Ok(*value)
        })
        .unwrap();

        assert_eq!(results, jobs);
        assert_eq!(peak.load(Ordering::SeqCst), workers);
    }

    #[test]
    fn ordered_workers_cancel_unclaimed_work_and_report_earliest_input_error() {
        let jobs = (0..256).collect::<Vec<_>>();
        let workers = artifact_worker_count(jobs.len()).unwrap();
        let barrier = Arc::new(std::sync::Barrier::new(workers));
        let claimed = AtomicUsize::new(0);

        let error = parallel_ordered(&jobs, |_, value| -> Result<usize, ArtifactInstallError> {
            claimed.fetch_add(1, Ordering::SeqCst);
            if *value < workers {
                barrier.wait();
            }
            if *value == workers - 1 {
                return Err(ArtifactInstallError::Download {
                    url: format!("error-{value}"),
                    reason: "later input failed first".to_string(),
                });
            }
            if *value == 0 {
                std::thread::sleep(Duration::from_millis(20));
                return Err(ArtifactInstallError::Download {
                    url: "error-0".to_string(),
                    reason: "earliest input failed later".to_string(),
                });
            }
            std::thread::sleep(Duration::from_millis(40));
            Ok(*value)
        })
        .unwrap_err();

        assert!(matches!(
            error,
            ArtifactInstallError::Download { url, reason }
                if url == "error-0" && reason == "earliest input failed later"
        ));
        assert!(claimed.load(Ordering::SeqCst) < jobs.len());
    }
}
