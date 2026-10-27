use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::io::{Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::curseforge::model::{CfFile, CfProject};
use crate::curseforge::resolver::{CurseForgeApi, ResolveError};

const CACHE_SCHEMA_VERSION: u32 = 1;
const CACHE_RELATIVE_DIR: &str = ".mcmpu/cache/curseforge-api";
const PROJECT_TTL: Duration = Duration::from_secs(24 * 60 * 60);
const FILE_TTL: Duration = Duration::from_secs(24 * 60 * 60);
const FILE_LIST_TTL: Duration = Duration::from_secs(10 * 60);
const CACHE_LOCK_TIMEOUT: Duration = Duration::from_secs(120);

static TEMP_FILE_SEQUENCE: AtomicU64 = AtomicU64::new(0);
static REQUEST_LOCKS: OnceLock<Mutex<HashMap<PathBuf, Weak<Mutex<()>>>>> = OnceLock::new();

/// Controls the project-local CurseForge response cache. The non-default fields are intended for
/// deterministic tests and internal maintenance operations, not user configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CurseForgeCacheOptions {
    pub enabled: bool,
    pub refresh: bool,
    pub project_ttl: Duration,
    pub file_ttl: Duration,
    pub file_list_ttl: Duration,
}

impl Default for CurseForgeCacheOptions {
    fn default() -> Self {
        Self {
            enabled: true,
            refresh: false,
            project_ttl: PROJECT_TTL,
            file_ttl: FILE_TTL,
            file_list_ttl: FILE_LIST_TTL,
        }
    }
}

impl CurseForgeCacheOptions {
    pub fn disabled() -> Self {
        Self {
            enabled: false,
            ..Self::default()
        }
    }

    pub fn refresh() -> Self {
        Self {
            refresh: true,
            ..Self::default()
        }
    }
}

/// Adds an optional, project-local persistent cache to any CurseForge API implementation.
/// API credentials are deliberately absent from cache keys and serialized cache records.
pub struct PersistentCurseForgeApi<'a, A> {
    api: &'a A,
    cache_root: Option<PathBuf>,
    options: CurseForgeCacheOptions,
}

impl<'a, A> PersistentCurseForgeApi<'a, A> {
    pub fn new(api: &'a A, project_root: Option<&Path>) -> Self {
        Self::with_options(api, project_root, CurseForgeCacheOptions::default())
    }

    pub fn with_options(
        api: &'a A,
        project_root: Option<&Path>,
        options: CurseForgeCacheOptions,
    ) -> Self {
        Self {
            api,
            cache_root: project_root.map(|root| root.join(CACHE_RELATIVE_DIR)),
            options,
        }
    }

    fn cached<T, F>(
        &self,
        request: CacheRequest,
        ttl: Duration,
        fetch: F,
    ) -> Result<T, ResolveError>
    where
        T: Clone + Serialize + DeserializeOwned,
        F: FnOnce() -> Result<T, ResolveError>,
    {
        if !self.options.enabled {
            return fetch();
        }
        let Some(cache_root) = &self.cache_root else {
            return fetch();
        };
        create_cache_dir(cache_root)?;
        let canonical_root = fs::canonicalize(cache_root).map_err(|source| {
            cache_error(
                cache_root,
                format!("failed to resolve cache directory: {source}"),
            )
        })?;
        let path = cache_path(&canonical_root, &request)?;
        let request_lock = request_lock(&path)?;
        let _guard = request_lock
            .lock()
            .map_err(|_| cache_error(&path, "request lock was poisoned by an earlier failure"))?;
        let _file_lock = CacheRequestFileLock::acquire(&cache_lock_path(&canonical_root, &path)?)?;

        if !self.options.refresh
            && let Some(value) = read_fresh_cache(&path, &request, ttl)?
        {
            return Ok(value);
        }

        let value = fetch()?;
        write_cache(&path, &request, &value)?;
        Ok(value)
    }
}

fn cache_lock_path(cache_root: &Path, cache_path: &Path) -> Result<PathBuf, ResolveError> {
    let parent = cache_root
        .parent()
        .ok_or_else(|| cache_error(cache_root, "cache directory has no parent"))?;
    let lock_root = parent.join("curseforge-api-locks");
    fs::create_dir_all(&lock_root).map_err(|source| {
        cache_error(
            &lock_root,
            format!("failed to create cache lock directory: {source}"),
        )
    })?;
    let file_name = cache_path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| cache_error(cache_path, "cache file name is not valid UTF-8"))?;
    Ok(lock_root.join(format!("{file_name}.lock")))
}

struct CacheRequestFileLock {
    path: PathBuf,
    file: fs::File,
}

impl CacheRequestFileLock {
    fn acquire(path: &Path) -> Result<Self, ResolveError> {
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)
            .map_err(|source| cache_error(path, format!("failed to open cache lock: {source}")))?;
        let started = Instant::now();
        loop {
            match file.try_lock() {
                Ok(()) => {
                    file.set_len(0)
                        .and_then(|()| file.seek(SeekFrom::Start(0)))
                        .and_then(|_| writeln!(file, "pid={}", std::process::id()))
                        .and_then(|()| file.sync_all())
                        .map_err(|source| {
                            cache_error(path, format!("failed to initialize cache lock: {source}"))
                        })?;
                    return Ok(Self {
                        path: path.to_path_buf(),
                        file,
                    });
                }
                Err(std::fs::TryLockError::WouldBlock)
                    if started.elapsed() < CACHE_LOCK_TIMEOUT =>
                {
                    thread::sleep(Duration::from_millis(100));
                }
                Err(std::fs::TryLockError::WouldBlock) => {
                    return Err(cache_error(
                        path,
                        "timed out waiting for cache request lock",
                    ));
                }
                Err(std::fs::TryLockError::Error(source)) => {
                    return Err(cache_error(
                        path,
                        format!("failed to acquire cache request lock: {source}"),
                    ));
                }
            }
        }
    }
}

impl Drop for CacheRequestFileLock {
    fn drop(&mut self) {
        if let Err(error) = self.file.unlock() {
            eprintln!(
                "failed to release CurseForge cache lock '{}': {error}",
                self.path.display()
            );
        }
    }
}

impl<A: CurseForgeApi> CurseForgeApi for PersistentCurseForgeApi<'_, A> {
    fn get_project(&self, project_id: u64) -> Result<CfProject, ResolveError> {
        self.cached(
            CacheRequest::GetProject { project_id },
            self.options.project_ttl,
            || self.api.get_project(project_id),
        )
    }

    fn get_file(&self, project_id: u64, file_id: u64) -> Result<CfFile, ResolveError> {
        self.cached(
            CacheRequest::GetFile {
                project_id,
                file_id,
            },
            self.options.file_ttl,
            || self.api.get_file(project_id, file_id),
        )
    }

    fn find_project_by_slug(&self, slug: &str) -> Result<CfProject, ResolveError> {
        self.cached(
            CacheRequest::FindProjectBySlug {
                slug: slug.to_string(),
            },
            self.options.project_ttl,
            || self.api.find_project_by_slug(slug),
        )
    }

    fn search_projects(&self, query: &str) -> Result<Vec<CfProject>, ResolveError> {
        self.api.search_projects(query)
    }

    fn list_files(
        &self,
        project_id: u64,
        minecraft_version: &str,
        loader: &str,
    ) -> Result<Vec<CfFile>, ResolveError> {
        self.cached(
            CacheRequest::ListFiles {
                project_id,
                minecraft_version: minecraft_version.to_string(),
                loader: loader.to_string(),
            },
            self.options.file_list_ttl,
            || self.api.list_files(project_id, minecraft_version, loader),
        )
    }

    fn list_project_files(&self, project_id: u64) -> Result<Vec<CfFile>, ResolveError> {
        self.cached(
            CacheRequest::ListProjectFiles { project_id },
            self.options.file_list_ttl,
            || self.api.list_project_files(project_id),
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "method", content = "parameters", rename_all = "snake_case")]
enum CacheRequest {
    GetProject {
        project_id: u64,
    },
    GetFile {
        project_id: u64,
        file_id: u64,
    },
    FindProjectBySlug {
        slug: String,
    },
    ListFiles {
        project_id: u64,
        minecraft_version: String,
        loader: String,
    },
    ListProjectFiles {
        project_id: u64,
    },
}

impl CacheRequest {
    fn method_name(&self) -> &'static str {
        match self {
            Self::GetProject { .. } => "get-project",
            Self::GetFile { .. } => "get-file",
            Self::FindProjectBySlug { .. } => "find-project-by-slug",
            Self::ListFiles { .. } => "list-files",
            Self::ListProjectFiles { .. } => "list-project-files",
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CacheEnvelope<T> {
    version: u32,
    fetched_at: u64,
    request: CacheRequest,
    value: T,
}

fn create_cache_dir(path: &Path) -> Result<(), ResolveError> {
    fs::create_dir_all(path)
        .map_err(|source| cache_error(path, format!("failed to create cache directory: {source}")))
}

fn cache_path(root: &Path, request: &CacheRequest) -> Result<PathBuf, ResolveError> {
    let encoded = serde_json::to_vec(request).map_err(|source| {
        cache_error(
            root,
            format!("failed to encode request cache key: {source}"),
        )
    })?;
    let digest = format!("{:x}", Sha256::digest(encoded));
    Ok(root.join(format!("{}-{digest}.json", request.method_name())))
}

fn request_lock(path: &Path) -> Result<Arc<Mutex<()>>, ResolveError> {
    let locks = REQUEST_LOCKS.get_or_init(|| Mutex::new(HashMap::new()));
    let mut locks = locks
        .lock()
        .map_err(|_| cache_error(path, "global request lock registry was poisoned"))?;
    if let Some(lock) = locks.get(path).and_then(Weak::upgrade) {
        return Ok(lock);
    }
    locks.retain(|_, lock| lock.strong_count() > 0);
    let lock = Arc::new(Mutex::new(()));
    locks.insert(path.to_path_buf(), Arc::downgrade(&lock));
    Ok(lock)
}

fn read_fresh_cache<T>(
    path: &Path,
    expected_request: &CacheRequest,
    ttl: Duration,
) -> Result<Option<T>, ResolveError>
where
    T: DeserializeOwned,
{
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(source) => {
            return Err(cache_error(
                path,
                format!("failed to read cache record: {source}"),
            ));
        }
    };
    let envelope: CacheEnvelope<T> = serde_json::from_slice(&bytes)
        .map_err(|source| cache_error(path, format!("failed to parse cache record: {source}")))?;
    if envelope.version != CACHE_SCHEMA_VERSION {
        return Err(cache_error(
            path,
            format!(
                "unsupported cache schema version {}; expected {CACHE_SCHEMA_VERSION}",
                envelope.version
            ),
        ));
    }
    if &envelope.request != expected_request {
        return Err(cache_error(
            path,
            "cache request key does not match its file name",
        ));
    }
    let now = unix_timestamp()?;
    let age = now.checked_sub(envelope.fetched_at).ok_or_else(|| {
        cache_error(
            path,
            format!(
                "cache timestamp {} is in the future relative to {now}",
                envelope.fetched_at
            ),
        )
    })?;
    if age >= ttl.as_secs() {
        return Ok(None);
    }
    Ok(Some(envelope.value))
}

fn write_cache<T>(path: &Path, request: &CacheRequest, value: &T) -> Result<(), ResolveError>
where
    T: Serialize,
{
    let envelope = CacheEnvelope {
        version: CACHE_SCHEMA_VERSION,
        fetched_at: unix_timestamp()?,
        request: request.clone(),
        value,
    };
    let bytes = serde_json::to_vec_pretty(&envelope).map_err(|source| {
        cache_error(path, format!("failed to serialize cache record: {source}"))
    })?;
    atomic_replace(path, &bytes)
}

fn unix_timestamp() -> Result<u64, ResolveError> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .map_err(|source| ResolveError::Cache {
            path: "system clock".to_string(),
            message: format!("time is before the Unix epoch: {source}"),
        })
}

fn atomic_replace(path: &Path, bytes: &[u8]) -> Result<(), ResolveError> {
    let parent = path
        .parent()
        .ok_or_else(|| cache_error(path, "cache path has no parent directory"))?;
    let sequence = TEMP_FILE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| cache_error(path, "cache file name is not valid UTF-8"))?;
    let part = parent.join(format!(
        ".{file_name}.{}.{}.part",
        std::process::id(),
        sequence
    ));
    let result = (|| {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&part)
            .map_err(|source| {
                cache_error(
                    &part,
                    format!("failed to create temporary record: {source}"),
                )
            })?;
        file.write_all(bytes).map_err(|source| {
            cache_error(&part, format!("failed to write temporary record: {source}"))
        })?;
        file.sync_all().map_err(|source| {
            cache_error(&part, format!("failed to sync temporary record: {source}"))
        })?;
        drop(file);
        replace_file(&part, path).map_err(|source| {
            cache_error(
                path,
                format!("failed to atomically replace cache record: {source}"),
            )
        })
    })();
    if result.is_err() {
        match fs::remove_file(&part) {
            Ok(()) => {}
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => {}
            Err(cleanup_error) => {
                return Err(cache_error(
                    &part,
                    format!(
                        "cache write failed and temporary cleanup also failed: {cleanup_error}"
                    ),
                ));
            }
        }
    }
    result
}

#[cfg(not(windows))]
fn replace_file(source: &Path, destination: &Path) -> std::io::Result<()> {
    fs::rename(source, destination)
}

#[cfg(windows)]
fn replace_file(source: &Path, destination: &Path) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt;

    const MOVEFILE_REPLACE_EXISTING: u32 = 0x1;
    const MOVEFILE_WRITE_THROUGH: u32 = 0x8;

    unsafe extern "system" {
        fn MoveFileExW(
            existing_file_name: *const u16,
            new_file_name: *const u16,
            flags: u32,
        ) -> i32;
    }

    let source: Vec<u16> = source
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let destination: Vec<u16> = destination
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let moved = unsafe {
        MoveFileExW(
            source.as_ptr(),
            destination.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if moved == 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

fn cache_error(path: &Path, message: impl Into<String>) -> ResolveError {
    ResolveError::Cache {
        path: path.display().to_string(),
        message: message.into(),
    }
}
