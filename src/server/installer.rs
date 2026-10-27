use std::collections::HashSet;
use std::error::Error as StdError;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};

use reqwest::Url;
use reqwest::header::{ACCEPT, HeaderValue};
use semver::Version;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::net::{
    ArtifactRequest, ArtifactTransfer, HttpRequest, HttpTransport, NetworkConfig, NetworkEngine,
    NetworkError, TransferError, TransferObserver,
};

const RELEASES_API_URL: &str =
    "https://api.github.com/repos/Circulate233/MCServerDownloadTool/releases";
const LATEST_RELEASE_INDEX_URL: &str = "https://github.com/Circulate233/MCServerDownloadTool/releases/latest/download/release-index.json";
const RELEASE_ASSET_API_PREFIX: &str =
    "https://api.github.com/repos/Circulate233/MCServerDownloadTool/releases/assets";
const RELEASE_DOWNLOAD_PREFIX: &str =
    "https://github.com/Circulate233/MCServerDownloadTool/releases/download";
const RELEASE_PACKAGE: &str = "mc-server-download-tool";
const RELEASES_PER_PAGE: usize = 100;
const MAX_RELEASE_PAGES: usize = 10;
const MAX_RELEASES: usize = RELEASES_PER_PAGE * MAX_RELEASE_PAGES;
const MAX_RELEASES_RESPONSE_BYTES: usize = 2 * 1024 * 1024;
const MAX_RELEASE_ASSETS: usize = 64;
const MAX_RELEASE_INDEX_BYTES: u64 = 8 * 1024 * 1024;
const MAX_LATEST_RELEASE_RESPONSE_BYTES: usize = 2 * 1024 * 1024;

const EXPECTED_ASSET_SPECS: [ExpectedAssetSpec; 3] = [
    ExpectedAssetSpec {
        name: "MCServerDownloadTool-linux-x86_64",
        platform: "linux-x86_64",
        target: "x86_64-unknown-linux-musl",
        executable: true,
    },
    ExpectedAssetSpec {
        name: "MCServerDownloadTool-macos-aarch64",
        platform: "macos-aarch64",
        target: "aarch64-apple-darwin",
        executable: true,
    },
    ExpectedAssetSpec {
        name: "MCServerDownloadTool-windows-x86_64.exe",
        platform: "windows-x86_64",
        target: "x86_64-pc-windows-msvc",
        executable: false,
    },
];

static ACTIVE_PREPARES: OnceLock<(Mutex<HashSet<PathBuf>>, Condvar)> = OnceLock::new();
static CACHE_STATE_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// One validated installer executable ready to be embedded in the server ZIP.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerInstallerAsset {
    /// Target platform this executable was published for.
    pub platform: ServerPlatform,
    /// Original release asset name retained in the archive.
    pub name: String,
    /// Verified cache file streamed into the server archive.
    pub path: PathBuf,
    /// Exact release asset size.
    pub size: u64,
    /// Exact release asset SHA-256.
    pub sha256: String,
    /// Whether the ZIP entry requires Unix mode 0755.
    pub unix_executable: bool,
}

impl ServerInstallerAsset {
    /// Creates one platform-bound installer asset and validates its immutable identity.
    pub fn new(
        platform: ServerPlatform,
        path: PathBuf,
        size: u64,
        sha256: String,
    ) -> Result<Self, ServerInstallerError> {
        if size == 0 || !valid_sha256(&sha256) {
            return Err(ServerInstallerError::AssetSet {
                reason: "installer asset must have a non-zero size and valid SHA-256".to_string(),
            });
        }
        let spec = platform.expected_spec();
        Ok(Self {
            platform,
            name: spec.name.to_string(),
            path,
            size,
            sha256: sha256.to_ascii_lowercase(),
            unix_executable: spec.executable,
        })
    }

    /// Verifies that a source returned the exact asset requested by the builder.
    pub fn validate_for(&self, platform: ServerPlatform) -> Result<(), ServerInstallerError> {
        let spec = platform.expected_spec();
        if self.platform != platform
            || self.name != spec.name
            || self.unix_executable != spec.executable
            || self.size == 0
            || !valid_sha256(&self.sha256)
        {
            return Err(ServerInstallerError::AssetSet {
                reason: format!(
                    "prepared installer asset does not match requested platform '{}'",
                    platform.as_str()
                ),
            });
        }
        Ok(())
    }
}

/// Supported operating-system and architecture pairs for generated server packages.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ServerPlatform {
    /// 64-bit Windows using the MSVC executable.
    WindowsX86_64,
    /// 64-bit Linux using the statically linked musl executable.
    LinuxX86_64,
    /// Apple Silicon macOS executable.
    MacosAarch64,
}

impl ServerPlatform {
    /// Resolves the current compilation target, rejecting unsupported host combinations.
    pub fn current() -> Result<Self, ServerPlatformError> {
        Self::from_target_parts(std::env::consts::OS, std::env::consts::ARCH)
    }

    fn from_target_parts(os: &str, arch: &str) -> Result<Self, ServerPlatformError> {
        match (os, arch) {
            ("windows", "x86_64") => Ok(Self::WindowsX86_64),
            ("linux", "x86_64") => Ok(Self::LinuxX86_64),
            ("macos", "aarch64") => Ok(Self::MacosAarch64),
            (os, arch) => Err(ServerPlatformError {
                os: os.to_string(),
                arch: arch.to_string(),
            }),
        }
    }

    /// Stable platform identifier used in CLI values and server archive names.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::WindowsX86_64 => "windows-x86_64",
            Self::LinuxX86_64 => "linux-x86_64",
            Self::MacosAarch64 => "macos-aarch64",
        }
    }

    fn expected_spec(self) -> &'static ExpectedAssetSpec {
        let platform = self.as_str();
        EXPECTED_ASSET_SPECS
            .iter()
            .find(|spec| spec.platform == platform)
            .expect("every public server platform must have an installer asset specification")
    }
}

/// Error returned when the current host cannot produce a supported server package by default.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("unsupported current server platform '{os}-{arch}'")]
pub struct ServerPlatformError {
    /// Operating-system identifier reported by Rust.
    pub os: String,
    /// Architecture identifier reported by Rust.
    pub arch: String,
}

/// Validated installer-release provider consumed by the project builder.
///
/// Implementations own acquisition and integrity verification. A successful result is safe to
/// embed directly; callers must not reinterpret remote metadata or silently replace assets.
pub trait ServerInstallerSource {
    /// Concrete acquisition error retained as the cause of a build failure.
    type Error: StdError + Send + Sync + 'static;

    /// Starts the metadata-only request phase used by a dual client/server build.
    ///
    /// `request_started` must be invoked exactly once immediately before the first remote
    /// metadata request (or immediately when no request is required). The returned request must
    /// not transfer installer payloads, hash cache files, or write installer cache state until
    /// [`ServerInstallerRequest::prepare`] is called.
    fn request(
        &self,
        platform: ServerPlatform,
        request_started: &mut dyn FnMut(),
    ) -> Result<Box<dyn ServerInstallerRequest<Error = Self::Error> + '_>, Self::Error>
    where
        Self: Sized + Sync,
    {
        request_started();
        Ok(Box::new(DeferredServerInstallerRequest {
            source: self,
            platform,
        }))
    }

    /// Validates the complete release contract and prepares only the requested platform asset.
    fn prepare(
        &self,
        project_dir: &Path,
        platform: ServerPlatform,
    ) -> Result<ServerInstallerAsset, Self::Error>;
}

/// Metadata request awaiting its payload/cache preparation phase.
pub trait ServerInstallerRequest: Send {
    /// Concrete preparation error retained as the cause of a build failure.
    type Error: StdError + Send + Sync + 'static;

    /// Downloads, verifies, and caches the requested installer payload.
    fn prepare(self: Box<Self>, project_dir: &Path) -> Result<ServerInstallerAsset, Self::Error>;
}

struct DeferredServerInstallerRequest<'a, S> {
    source: &'a S,
    platform: ServerPlatform,
}

impl<S> ServerInstallerRequest for DeferredServerInstallerRequest<'_, S>
where
    S: ServerInstallerSource + Sync,
{
    type Error = S::Error;

    fn prepare(self: Box<Self>, project_dir: &Path) -> Result<ServerInstallerAsset, Self::Error> {
        self.source.prepare(project_dir, self.platform)
    }
}

/// HTTP release source backed by the shared bounded artifact transfer engine.
pub struct HttpServerInstallerSource<T = NetworkEngine> {
    transfer: T,
}

impl HttpServerInstallerSource<NetworkEngine> {
    /// Creates a production source whose HTTP requests inherit the effective build proxy.
    pub fn with_proxy(proxy: Option<&str>) -> Result<Self, NetworkError> {
        let transfer = NetworkEngine::new(NetworkConfig {
            proxy: proxy.map(str::to_string),
            ..NetworkConfig::default()
        })?;
        Ok(Self { transfer })
    }
}

enum HttpServerInstallerRequest<'a, T> {
    Api {
        transfer: &'a T,
        contract: ReleaseContract,
        platform: ServerPlatform,
    },
    Direct {
        transfer: &'a T,
        tag: String,
        latest_index: Vec<u8>,
        api_summary: String,
        platform: ServerPlatform,
    },
}

impl<T> ServerInstallerRequest for HttpServerInstallerRequest<'_, T>
where
    T: ArtifactTransfer + HttpTransport,
{
    type Error = ServerInstallerError;

    fn prepare(self: Box<Self>, project_dir: &Path) -> Result<ServerInstallerAsset, Self::Error> {
        match *self {
            Self::Api {
                transfer,
                contract,
                platform,
            } => prepare_release(transfer, project_dir, &contract, platform),
            Self::Direct {
                transfer,
                tag,
                latest_index,
                api_summary,
                platform,
            } => prepare_direct_release(
                transfer,
                project_dir,
                &tag,
                Some(&latest_index),
                platform,
            )
            .map_err(|direct_error| ServerInstallerError::ReleaseMetadata {
                reason: format!(
                    "{api_summary}; direct verified release fallback for '{tag}' failed: {direct_error}"
                ),
            }),
        }
    }
}

impl<T> ServerInstallerSource for HttpServerInstallerSource<T>
where
    T: ArtifactTransfer + HttpTransport,
{
    type Error = ServerInstallerError;

    fn request(
        &self,
        platform: ServerPlatform,
        request_started: &mut dyn FnMut(),
    ) -> Result<Box<dyn ServerInstallerRequest<Error = Self::Error> + '_>, Self::Error> {
        request_started();
        match discover_latest_stable_release(&self.transfer) {
            Ok(release) => Ok(Box::new(HttpServerInstallerRequest::Api {
                transfer: &self.transfer,
                contract: release_contract(&release)?,
                platform,
            })),
            Err(api_error) if is_recoverable_releases_api_failure(&api_error) => {
                let api_summary = releases_api_failure_summary(&api_error);
                let request =
                    HttpRequest::get(LATEST_RELEASE_INDEX_URL, MAX_LATEST_RELEASE_RESPONSE_BYTES)
                        .build()?;
                let response = self.transfer.get_bytes(request).map_err(|latest_error| {
                    ServerInstallerError::ReleaseMetadata {
                        reason: format!(
                            "{api_summary}; GitHub browser latest release-index download also failed: {latest_error}"
                        ),
                    }
                })?;
                let tag = latest_direct_release_tag(&response.body)?;
                validate_latest_release_index_final_url(&response.final_url, &tag)?;
                Ok(Box::new(HttpServerInstallerRequest::Direct {
                    transfer: &self.transfer,
                    tag,
                    latest_index: response.body,
                    api_summary,
                    platform,
                }))
            }
            Err(error) => Err(error),
        }
    }

    fn prepare(
        &self,
        project_dir: &Path,
        platform: ServerPlatform,
    ) -> Result<ServerInstallerAsset, Self::Error> {
        let request = self.request(platform, &mut || {})?;
        request.prepare(project_dir)
    }
}

/// Installer release acquisition and integrity failures.
#[derive(Debug, Error)]
pub enum ServerInstallerError {
    /// A download request could not be constructed.
    #[error("invalid server installer download request: {0}")]
    Request(#[from] NetworkError),
    /// A verified artifact transfer failed.
    #[error("server installer transfer failed: {0}")]
    Transfer(#[from] TransferError),
    /// A cache file could not be read or written.
    #[error("failed to access server installer cache '{path}': {source}")]
    CacheIo {
        path: String,
        #[source]
        source: std::io::Error,
    },
    /// GitHub release metadata was invalid or had no stable SemVer release.
    #[error("invalid MCServerDownloadTool release metadata: {reason}")]
    ReleaseMetadata { reason: String },
    /// The selected release index did not match its strict schema or release contract.
    #[error("invalid server installer release index: {reason}")]
    ReleaseIndex { reason: String },
    /// Prepared assets did not contain the exact supported platform set.
    #[error("invalid server installer asset set: {reason}")]
    AssetSet { reason: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReleaseContract {
    cache_version: String,
    schema_version: u32,
    package: String,
    version: String,
    tag: String,
    index: ReleaseArtifactMetadata,
    release_assets: Vec<ReleaseArtifactMetadata>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReleaseIndexCacheState {
    schema_version: u32,
    tag: String,
    asset_id: u64,
    api_url: String,
    browser_url: String,
    size: u64,
    sha256: String,
}

/// The direct fallback is allowed only after the GitHub REST endpoint is unavailable.
/// Its cache entry is intentionally still bound to a stable tag and exact browser URL.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct DirectReleaseContract<'a> {
    tag: &'a str,
    version: &'a str,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ExpectedAssetSpec {
    name: &'static str,
    platform: &'static str,
    target: &'static str,
    executable: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ExpectedAsset {
    name: String,
    version: String,
    platform: String,
    target: String,
    download_url: String,
    browser_url: String,
    asset_id: u64,
    size: u64,
    sha256: String,
    executable: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReleaseArtifactMetadata {
    id: u64,
    name: String,
    api_url: String,
    browser_url: String,
    size: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct GitHubRelease {
    tag_name: String,
    draft: bool,
    prerelease: bool,
    assets: Vec<GitHubReleaseAsset>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct GitHubReleaseAsset {
    id: u64,
    url: String,
    name: String,
    size: u64,
    browser_download_url: String,
}

fn discover_latest_stable_release<T: HttpTransport>(
    transport: &T,
) -> Result<GitHubRelease, ServerInstallerError> {
    let mut releases = Vec::new();
    for page in 1..=MAX_RELEASE_PAGES {
        let url = format!("{RELEASES_API_URL}?per_page={RELEASES_PER_PAGE}&page={page}");
        let request = HttpRequest::get(&url, MAX_RELEASES_RESPONSE_BYTES).build()?;
        let response = transport.get_bytes(request)?;
        validate_releases_api_url(&response.final_url, page)?;
        let page_releases: Vec<GitHubRelease> =
            serde_json::from_slice(&response.body).map_err(|source| {
                ServerInstallerError::ReleaseMetadata {
                    reason: format!("failed to parse GitHub releases page {page}: {source}"),
                }
            })?;
        if page_releases.len() > RELEASES_PER_PAGE {
            return Err(ServerInstallerError::ReleaseMetadata {
                reason: format!(
                    "GitHub releases page {page} contains {} entries, exceeding limit {RELEASES_PER_PAGE}",
                    page_releases.len()
                ),
            });
        }
        for release in &page_releases {
            if release.assets.len() > MAX_RELEASE_ASSETS {
                return Err(ServerInstallerError::ReleaseMetadata {
                    reason: format!(
                        "release '{}' contains {} assets, exceeding limit {MAX_RELEASE_ASSETS}",
                        release.tag_name,
                        release.assets.len()
                    ),
                });
            }
        }
        let page_len = page_releases.len();
        releases.extend(page_releases);
        if releases.len() > MAX_RELEASES {
            return Err(ServerInstallerError::ReleaseMetadata {
                reason: format!("release count exceeds limit {MAX_RELEASES}"),
            });
        }
        if page_len < RELEASES_PER_PAGE {
            return select_latest_stable_release(&releases);
        }
    }
    Err(ServerInstallerError::ReleaseMetadata {
        reason: format!(
            "GitHub returned at least {MAX_RELEASES} releases; refusing an incomplete version comparison"
        ),
    })
}

fn select_latest_stable_release(
    releases: &[GitHubRelease],
) -> Result<GitHubRelease, ServerInstallerError> {
    let mut candidates = Vec::new();
    for release in releases {
        if release.draft || release.prerelease {
            continue;
        }
        let Some(version_text) = release.tag_name.strip_prefix('v') else {
            continue;
        };
        let Ok(version) = Version::parse(version_text) else {
            continue;
        };
        if !version.pre.is_empty() {
            continue;
        }
        candidates.push((version, release));
    }
    let Some((latest_version, latest_release)) =
        candidates.iter().max_by(|left, right| left.0.cmp(&right.0))
    else {
        return Err(ServerInstallerError::ReleaseMetadata {
            reason: "no non-draft, non-prerelease v-prefixed SemVer release exists".to_string(),
        });
    };
    let same_precedence = candidates
        .iter()
        .filter(|(version, _)| {
            version.major == latest_version.major
                && version.minor == latest_version.minor
                && version.patch == latest_version.patch
                && version.pre == latest_version.pre
        })
        .map(|(_, release)| release.tag_name.as_str())
        .collect::<Vec<_>>();
    if same_precedence.len() != 1 {
        return Err(ServerInstallerError::ReleaseMetadata {
            reason: format!(
                "multiple stable release tags have the same SemVer precedence: {}",
                same_precedence.join(", ")
            ),
        });
    }
    Ok((*latest_release).clone())
}

fn is_recoverable_releases_api_failure(error: &ServerInstallerError) -> bool {
    matches!(
        error,
        ServerInstallerError::Request(
            NetworkError::HttpStatus {
                status: 403 | 429 | 500..=599,
                ..
            } | NetworkError::Request { .. }
                | NetworkError::ResponseRead { .. }
                | NetworkError::ResponseTooLarge { .. }
                | NetworkError::Cancelled { .. }
                | NetworkError::Worker { .. }
        )
    )
}

fn releases_api_failure_summary(error: &ServerInstallerError) -> String {
    match error {
        ServerInstallerError::Request(NetworkError::HttpStatus {
            url,
            status: 403,
            attempts,
        }) => format!(
            "GitHub REST API returned HTTP 403 after {attempts} attempt(s) for '{}' (rate limit or access policy; direct release download fallback will be used)",
            redact_installer_url(url)
        ),
        ServerInstallerError::Request(NetworkError::HttpStatus {
            url,
            status,
            attempts,
        }) => format!(
            "GitHub REST API returned HTTP {status} after {attempts} attempt(s) for '{}' (direct release download fallback will be used)",
            redact_installer_url(url)
        ),
        _ => format!(
            "GitHub REST API is unavailable ({error}; direct release download fallback will be used)"
        ),
    }
}

fn redact_installer_url(value: &str) -> String {
    let Ok(mut url) = Url::parse(value) else {
        return "<invalid-url>".to_string();
    };
    let _ = url.set_username("");
    let _ = url.set_password(None);
    url.set_query(None);
    url.set_fragment(None);
    url.to_string()
}

/// Obtains the current stable release through GitHub's browser asset endpoint.
///
/// This deliberately does not use `api.github.com`: the REST endpoint has a low
/// unauthenticated rate limit, while GitHub resolves `/releases/latest/download/...`
/// as a normal release-asset redirect. The downloaded index is the trust contract
/// for the tag and every platform asset; no tag is inferred from a CDN redirect.
/// The browser endpoint may redirect to GitHub's signed release-asset CDN. The
/// redirect target is not a version authority and may contain credentials, so it
/// is only origin-validated. The index tag/version remains the release contract.
fn validate_latest_release_index_final_url(
    url: &Url,
    tag: &str,
) -> Result<(), ServerInstallerError> {
    let trusted_github_download = url.scheme() == "https"
        && url.host_str() == Some("github.com")
        && url.port().is_none()
        && url.username().is_empty()
        && url.password().is_none()
        && url.path()
            == "/Circulate233/MCServerDownloadTool/releases/latest/download/release-index.json";
    let trusted_exact_tag_download = url.scheme() == "https"
        && url.host_str() == Some("github.com")
        && url.port().is_none()
        && url.username().is_empty()
        && url.password().is_none()
        && url.path()
            == format!(
                "/Circulate233/MCServerDownloadTool/releases/download/{tag}/release-index.json"
            );
    let trusted_release_asset_cdn = url.scheme() == "https"
        && url
            .host_str()
            .is_some_and(|host| host == "release-assets.githubusercontent.com")
        && url.port().is_none()
        && url.username().is_empty()
        && url.password().is_none();
    if trusted_github_download || trusted_exact_tag_download || trusted_release_asset_cdn {
        return Ok(());
    }
    Err(ServerInstallerError::ReleaseMetadata {
        reason: "GitHub browser latest release-index download redirected to an untrusted origin"
            .to_string(),
    })
}

fn prepare_direct_release<T: ArtifactTransfer + HttpTransport>(
    transport: &T,
    project_dir: &Path,
    tag: &str,
    latest_index: Option<&[u8]>,
    platform: ServerPlatform,
) -> Result<ServerInstallerAsset, ServerInstallerError> {
    let version = stable_tag_version(tag)?;
    let cache_dir = project_dir.join(".mcmpu/cache/server-installer").join(tag);
    create_dir_all(&cache_dir)?;
    let cache_identity =
        fs::canonicalize(&cache_dir).map_err(|source| cache_io(&cache_dir, source))?;
    let _process_guard = ProcessPrepareGuard::acquire(cache_identity)?;
    let lock_path = cache_dir.join(".prepare.lock");
    let lock_file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock_path)
        .map_err(|source| cache_io(&lock_path, source))?;
    fs2::FileExt::lock_exclusive(&lock_file).map_err(|source| cache_io(&lock_path, source))?;

    let index_path = cache_dir.join("release-index.json");
    let contract = DirectReleaseContract { tag, version };
    let index_bytes = match latest_index {
        Some(bytes) => {
            validate_direct_release_index(bytes, contract)?;
            write_cache_bytes(&index_path, bytes)?;
            write_direct_release_index_state(&cache_dir, contract, bytes)?;
            bytes.to_vec()
        }
        None => match cached_direct_release_index(&cache_dir, &index_path, contract)? {
            Some(bytes) => bytes,
            None => download_direct_release_index(transport, &cache_dir, &index_path, contract)?,
        },
    };
    let expected_assets = validate_direct_release_index(&index_bytes, contract)?;
    prepare_expected_asset(transport, &cache_dir, &expected_assets, platform)
}

/// Extracts a stable direct-release tag from the signed-by-origin release index.
///
/// The full strict validation happens again before either cache or asset use. This
/// initial parse exists only because the latest asset redirect itself cannot carry
/// the selected tag after GitHub redirects it to its CDN.
fn latest_direct_release_tag(bytes: &[u8]) -> Result<String, ServerInstallerError> {
    let index: ReleaseIndex =
        serde_json::from_slice(bytes).map_err(|source| ServerInstallerError::ReleaseIndex {
            reason: format!("latest direct release index is invalid: {source}"),
        })?;
    if index.schema_version != 1 || index.package != RELEASE_PACKAGE {
        return Err(ServerInstallerError::ReleaseIndex {
            reason: "latest direct release index has an unsupported schema or package".to_string(),
        });
    }
    let version = stable_tag_version(&index.tag)?;
    if index.version != version {
        return Err(ServerInstallerError::ReleaseIndex {
            reason: "latest direct release index tag and version disagree".to_string(),
        });
    }
    let contract = DirectReleaseContract {
        tag: &index.tag,
        version,
    };
    validate_direct_release_index(bytes, contract)?;
    Ok(index.tag)
}

fn stable_tag_version(tag: &str) -> Result<&str, ServerInstallerError> {
    let version = tag
        .strip_prefix('v')
        .ok_or_else(|| ServerInstallerError::ReleaseMetadata {
            reason: format!("release tag '{tag}' is not v-prefixed SemVer"),
        })?;
    let parsed = Version::parse(version).map_err(|_| ServerInstallerError::ReleaseMetadata {
        reason: format!("release tag '{tag}' is not v-prefixed SemVer"),
    })?;
    if !parsed.pre.is_empty() {
        return Err(ServerInstallerError::ReleaseMetadata {
            reason: format!("release tag '{tag}' is a prerelease"),
        });
    }
    Ok(version)
}

fn cached_direct_release_index(
    cache_dir: &Path,
    index_path: &Path,
    contract: DirectReleaseContract<'_>,
) -> Result<Option<Vec<u8>>, ServerInstallerError> {
    let state_path = cache_dir.join(".release-state.json");
    let Some(state) = read_release_index_state(&state_path)? else {
        return Ok(None);
    };
    if state.schema_version != 1
        || state.tag != contract.tag
        || state.size == 0
        || state.size > MAX_RELEASE_INDEX_BYTES
        || !valid_sha256(&state.sha256)
        || validate_release_download_url(&state.browser_url, contract.tag, "release-index.json")
            .is_err()
    {
        return Ok(None);
    }
    let Some((bytes, sha256)) = read_index_with_sha256(index_path, state.size)? else {
        return Ok(None);
    };
    if sha256 != state.sha256 {
        return Ok(None);
    }
    match validate_direct_release_index(&bytes, contract) {
        Ok(_) => Ok(Some(bytes)),
        Err(error) => {
            eprintln!(
                "[build] cached server installer release index failed direct-release validation; downloading it again: {error}"
            );
            Ok(None)
        }
    }
}

fn download_direct_release_index<T: HttpTransport>(
    transport: &T,
    cache_dir: &Path,
    index_path: &Path,
    contract: DirectReleaseContract<'_>,
) -> Result<Vec<u8>, ServerInstallerError> {
    let url = release_download_url(contract.tag, "release-index.json")?;
    let request = HttpRequest::get(&url, MAX_RELEASE_INDEX_BYTES as usize).build()?;
    let response =
        transport
            .get_bytes(request)
            .map_err(|source| ServerInstallerError::ReleaseMetadata {
                reason: format!(
                    "direct release index download for '{}' failed: {}",
                    contract.tag, source
                ),
            })?;
    if !response.status.is_success() {
        return Err(ServerInstallerError::ReleaseMetadata {
            reason: format!(
                "direct release index download for '{}' returned HTTP {}",
                contract.tag, response.status
            ),
        });
    }
    if response.body.is_empty() || response.body.len() > MAX_RELEASE_INDEX_BYTES as usize {
        return Err(ServerInstallerError::ReleaseIndex {
            reason: format!(
                "direct release index for '{}' has invalid bounded size {}",
                contract.tag,
                response.body.len()
            ),
        });
    }
    validate_direct_release_index(&response.body, contract)?;
    write_cache_bytes(index_path, &response.body)?;
    write_direct_release_index_state(cache_dir, contract, &response.body)?;
    Ok(response.body)
}

fn write_direct_release_index_state(
    cache_dir: &Path,
    contract: DirectReleaseContract<'_>,
    bytes: &[u8],
) -> Result<(), ServerInstallerError> {
    let url = release_download_url(contract.tag, "release-index.json")?;
    let state = ReleaseIndexCacheState {
        schema_version: 1,
        tag: contract.tag.to_string(),
        // Direct release downloads do not expose GitHub REST asset IDs. This value is never
        // used as an API URL and only records that the cache originated from the strict URL.
        asset_id: 0,
        api_url: url.clone(),
        browser_url: url,
        size: bytes.len() as u64,
        sha256: format!("{:x}", Sha256::digest(bytes)),
    };
    write_release_index_state(&cache_dir.join(".release-state.json"), &state)
}

fn release_download_url(tag: &str, asset_name: &str) -> Result<String, ServerInstallerError> {
    let value = format!("{RELEASE_DOWNLOAD_PREFIX}/{tag}/{asset_name}");
    validate_release_download_url(&value, tag, asset_name)?;
    Ok(value)
}

fn release_contract(release: &GitHubRelease) -> Result<ReleaseContract, ServerInstallerError> {
    if release.draft || release.prerelease {
        return Err(ServerInstallerError::ReleaseMetadata {
            reason: format!("selected release '{}' is not stable", release.tag_name),
        });
    }
    let version = release
        .tag_name
        .strip_prefix('v')
        .and_then(|value| Version::parse(value).ok().map(|parsed| (value, parsed)))
        .ok_or_else(|| ServerInstallerError::ReleaseMetadata {
            reason: format!(
                "selected release tag '{}' is not v-prefixed SemVer",
                release.tag_name
            ),
        })?;
    if !version.1.pre.is_empty() {
        return Err(ServerInstallerError::ReleaseMetadata {
            reason: format!("selected release '{}' is a prerelease", release.tag_name),
        });
    }
    if release.assets.len() > MAX_RELEASE_ASSETS {
        return Err(ServerInstallerError::ReleaseMetadata {
            reason: format!("release '{}' contains too many assets", release.tag_name),
        });
    }
    let index = unique_release_asset(release, "release-index.json")?;
    if index.id == 0 || index.size == 0 || index.size > MAX_RELEASE_INDEX_BYTES {
        return Err(ServerInstallerError::ReleaseMetadata {
            reason: format!(
                "release-index.json size {} is outside 1..={MAX_RELEASE_INDEX_BYTES}",
                index.size
            ),
        });
    }
    validate_release_download_url(
        &index.browser_download_url,
        &release.tag_name,
        "release-index.json",
    )?;
    validate_release_asset_api_url(&index.url, index.id)?;

    let mut release_assets = Vec::with_capacity(EXPECTED_ASSET_SPECS.len());
    for spec in EXPECTED_ASSET_SPECS {
        let asset = unique_release_asset(release, spec.name)?;
        if asset.id == 0 || asset.size == 0 {
            return Err(ServerInstallerError::ReleaseMetadata {
                reason: format!("release asset '{}' has invalid id or size", spec.name),
            });
        }
        validate_release_download_url(&asset.browser_download_url, &release.tag_name, spec.name)?;
        validate_release_asset_api_url(&asset.url, asset.id)?;
        release_assets.push(ReleaseArtifactMetadata {
            id: asset.id,
            name: spec.name.to_string(),
            api_url: asset.url.clone(),
            browser_url: asset.browser_download_url.clone(),
            size: asset.size,
        });
    }
    Ok(ReleaseContract {
        cache_version: release.tag_name.clone(),
        schema_version: 1,
        package: RELEASE_PACKAGE.to_string(),
        version: version.0.to_string(),
        tag: release.tag_name.clone(),
        index: ReleaseArtifactMetadata {
            id: index.id,
            name: index.name.clone(),
            api_url: index.url.clone(),
            browser_url: index.browser_download_url.clone(),
            size: index.size,
        },
        release_assets,
    })
}

fn validate_release_asset_api_url(value: &str, asset_id: u64) -> Result<(), ServerInstallerError> {
    if asset_id == 0 {
        return Err(ServerInstallerError::ReleaseMetadata {
            reason: "release asset ID must be non-zero".to_string(),
        });
    }
    let expected = format!("{RELEASE_ASSET_API_PREFIX}/{asset_id}");
    if value != expected {
        return Err(ServerInstallerError::ReleaseMetadata {
            reason: format!("release asset API URL must be '{expected}', found '{value}'"),
        });
    }
    let url = Url::parse(value).map_err(|source| ServerInstallerError::ReleaseMetadata {
        reason: format!("invalid release asset API URL '{value}': {source}"),
    })?;
    if url.scheme() != "https"
        || url.host_str() != Some("api.github.com")
        || url.port().is_some()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(ServerInstallerError::ReleaseMetadata {
            reason: format!("release asset API URL '{value}' is not strict GitHub HTTPS"),
        });
    }
    Ok(())
}

fn unique_release_asset<'a>(
    release: &'a GitHubRelease,
    name: &str,
) -> Result<&'a GitHubReleaseAsset, ServerInstallerError> {
    let mut matching = release.assets.iter().filter(|asset| asset.name == name);
    let asset = matching
        .next()
        .ok_or_else(|| ServerInstallerError::ReleaseMetadata {
            reason: format!("release '{}' is missing asset '{name}'", release.tag_name),
        })?;
    if matching.next().is_some() {
        return Err(ServerInstallerError::ReleaseMetadata {
            reason: format!(
                "release '{}' contains duplicate asset '{name}'",
                release.tag_name
            ),
        });
    }
    Ok(asset)
}

fn validate_releases_api_url(url: &Url, page: usize) -> Result<(), ServerInstallerError> {
    let expected = format!("{RELEASES_API_URL}?per_page={RELEASES_PER_PAGE}&page={page}");
    if url.scheme() != "https"
        || url.host_str() != Some("api.github.com")
        || url.port().is_some()
        || url.path() != "/repos/Circulate233/MCServerDownloadTool/releases"
        || url.as_str() != expected
    {
        return Err(ServerInstallerError::ReleaseMetadata {
            reason: "GitHub releases API redirected to an unexpected origin or path".to_string(),
        });
    }
    Ok(())
}

fn validate_release_download_url(
    value: &str,
    tag: &str,
    asset_name: &str,
) -> Result<(), ServerInstallerError> {
    let expected = format!("{RELEASE_DOWNLOAD_PREFIX}/{tag}/{asset_name}");
    if value != expected {
        return Err(ServerInstallerError::ReleaseMetadata {
            reason: format!(
                "release asset '{asset_name}' URL must be the exact selected GitHub release path"
            ),
        });
    }
    let url = Url::parse(value).map_err(|source| ServerInstallerError::ReleaseMetadata {
        reason: format!("invalid release asset URL: {source}"),
    })?;
    if url.scheme() != "https"
        || url.host_str() != Some("github.com")
        || url.port().is_some()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(ServerInstallerError::ReleaseMetadata {
            reason: "release asset URL is not strict GitHub HTTPS".to_string(),
        });
    }
    Ok(())
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ReleaseIndex {
    schema_version: u32,
    package: String,
    version: String,
    tag: String,
    commit: String,
    assets: Vec<ReleaseAsset>,
    metadata: ReleaseMetadata,
}

#[derive(Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct ReleaseAsset {
    name: String,
    version: String,
    platform: String,
    target: String,
    size: u64,
    sha256: String,
    url: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReleaseMetadata {
    provenance: String,
    workflow: String,
}

fn prepare_release<T: ArtifactTransfer>(
    transfer: &T,
    project_dir: &Path,
    contract: &ReleaseContract,
    platform: ServerPlatform,
) -> Result<ServerInstallerAsset, ServerInstallerError> {
    let cache_dir = project_dir
        .join(".mcmpu/cache/server-installer")
        .join(&contract.cache_version);
    create_dir_all(&cache_dir)?;
    let cache_identity =
        fs::canonicalize(&cache_dir).map_err(|source| cache_io(&cache_dir, source))?;
    let _process_guard = ProcessPrepareGuard::acquire(cache_identity)?;
    let lock_path = cache_dir.join(".prepare.lock");
    let lock_file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock_path)
        .map_err(|source| cache_io(&lock_path, source))?;
    fs2::FileExt::lock_exclusive(&lock_file).map_err(|source| cache_io(&lock_path, source))?;

    let index_path = cache_dir.join("release-index.json");
    let index_bytes = ensure_dynamic_release_index(transfer, &cache_dir, &index_path, contract)?;
    let expected_assets = validate_release_index(&index_bytes, contract)?;

    prepare_expected_asset(transfer, &cache_dir, &expected_assets, platform)
}

fn prepare_expected_asset<T: ArtifactTransfer>(
    transfer: &T,
    cache_dir: &Path,
    expected_assets: &[ExpectedAsset],
    platform: ServerPlatform,
) -> Result<ServerInstallerAsset, ServerInstallerError> {
    let spec = platform.expected_spec();
    let asset = expected_assets
        .iter()
        .find(|asset| asset.name == spec.name)
        .ok_or_else(|| ServerInstallerError::AssetSet {
            reason: format!(
                "validated release does not contain requested platform '{}'",
                platform.as_str()
            ),
        })?;
    let path = cache_dir.join(&asset.name);
    if !cached_file_matches(&path, asset.size, &asset.sha256)? {
        let request = ArtifactRequest::builder(&asset.name, &path, &asset.download_url)
            .header(ACCEPT, HeaderValue::from_static("application/octet-stream"))?
            .expected_size(asset.size)
            .expected_sha256(&asset.sha256)
            .build()?;
        let results = transfer.transfer_many(vec![request], no_progress_observer());
        for result in results {
            result?;
        }
    }

    if !cached_file_matches(&path, asset.size, &asset.sha256)? {
        return Err(ServerInstallerError::AssetSet {
            reason: format!(
                "asset '{}' failed final size or SHA-256 verification after transfer",
                asset.name
            ),
        });
    }
    let prepared = ServerInstallerAsset::new(platform, path, asset.size, asset.sha256.clone())?;
    prepared.validate_for(platform)?;
    Ok(prepared)
}

fn ensure_dynamic_release_index<T: ArtifactTransfer>(
    transfer: &T,
    cache_dir: &Path,
    index_path: &Path,
    contract: &ReleaseContract,
) -> Result<Vec<u8>, ServerInstallerError> {
    let state_path = cache_dir.join(".release-state.json");
    if let Some(state) = read_release_index_state(&state_path)? {
        if state.schema_version == 1
            && state.tag == contract.tag
            && state.asset_id == contract.index.id
            && state.api_url == contract.index.api_url
            && state.browser_url == contract.index.browser_url
            && state.size == contract.index.size
            && valid_sha256(&state.sha256)
        {
            if let Some((bytes, sha256)) = read_index_with_sha256(index_path, contract.index.size)?
            {
                if sha256 == state.sha256 {
                    return Ok(bytes);
                }
                eprintln!(
                    "[build] cached server installer release index failed SHA-256 validation; downloading it again"
                );
            }
        } else {
            eprintln!(
                "[build] cached server installer release state does not match the selected GitHub release; downloading its index again"
            );
        }
    }

    let request = ArtifactRequest::builder(
        "server-installer-release-index",
        index_path,
        &contract.index.api_url,
    )
    .header(ACCEPT, HeaderValue::from_static("application/octet-stream"))?
    .expected_size(contract.index.size)
    .build()?;
    transfer.transfer_one(request, no_progress_observer())?;
    let Some((bytes, sha256)) = read_index_with_sha256(index_path, contract.index.size)? else {
        return Err(ServerInstallerError::ReleaseIndex {
            reason: "release-index.json was not materialized as the exact regular file declared by GitHub"
                .to_string(),
        });
    };
    validate_release_index(&bytes, contract)?;
    let state = ReleaseIndexCacheState {
        schema_version: 1,
        tag: contract.tag.clone(),
        asset_id: contract.index.id,
        api_url: contract.index.api_url.clone(),
        browser_url: contract.index.browser_url.clone(),
        size: contract.index.size,
        sha256,
    };
    write_release_index_state(&state_path, &state)?;
    Ok(bytes)
}

fn read_release_index_state(
    path: &Path,
) -> Result<Option<ReleaseIndexCacheState>, ServerInstallerError> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(source) => return Err(cache_io(path, source)),
    };
    if !is_regular_cache_file(&metadata) || metadata.len() > 16 * 1024 {
        eprintln!(
            "[build] cached server installer release state is not a bounded regular file; rebuilding it"
        );
        return Ok(None);
    }
    let bytes = read_file(path)?;
    match serde_json::from_slice(&bytes) {
        Ok(state) => Ok(Some(state)),
        Err(error) => {
            eprintln!(
                "[build] cached server installer release state is invalid JSON ({error}); rebuilding it"
            );
            Ok(None)
        }
    }
}

fn read_index_with_sha256(
    path: &Path,
    expected_size: u64,
) -> Result<Option<(Vec<u8>, String)>, ServerInstallerError> {
    let path_metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(source) => return Err(cache_io(path, source)),
    };
    if !is_regular_cache_file(&path_metadata) || path_metadata.len() != expected_size {
        return Ok(None);
    }
    let mut file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(source) => return Err(cache_io(path, source)),
    };
    let metadata = file.metadata().map_err(|source| cache_io(path, source))?;
    if !is_regular_cache_file(&metadata)
        || metadata.len() != expected_size
        || metadata.len() != path_metadata.len()
        || metadata.len() > MAX_RELEASE_INDEX_BYTES
    {
        return Ok(None);
    }
    let capacity =
        usize::try_from(expected_size).map_err(|_| ServerInstallerError::ReleaseIndex {
            reason: "release-index.json size does not fit memory address space".to_string(),
        })?;
    let mut bytes = Vec::with_capacity(capacity);
    file.read_to_end(&mut bytes)
        .map_err(|source| cache_io(path, source))?;
    if bytes.len() != capacity {
        return Ok(None);
    }
    let sha256 = format!("{:x}", Sha256::digest(&bytes));
    Ok(Some((bytes, sha256)))
}

fn write_release_index_state(
    path: &Path,
    state: &ReleaseIndexCacheState,
) -> Result<(), ServerInstallerError> {
    let bytes =
        serde_json::to_vec_pretty(state).map_err(|source| ServerInstallerError::ReleaseIndex {
            reason: format!("failed to serialize release cache state: {source}"),
        })?;
    write_cache_bytes(path, &bytes)
}

fn write_cache_bytes(path: &Path, bytes: &[u8]) -> Result<(), ServerInstallerError> {
    let parent = path
        .parent()
        .ok_or_else(|| ServerInstallerError::ReleaseIndex {
            reason: format!(
                "server installer cache file '{}' has no parent",
                path.display()
            ),
        })?;
    let sequence = CACHE_STATE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let temporary = parent.join(format!(
        ".server-installer.{}.{}.tmp",
        std::process::id(),
        sequence
    ));
    let result = (|| {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .truncate(true)
            .open(&temporary)
            .map_err(|source| cache_io(&temporary, source))?;
        file.write_all(bytes)
            .and_then(|_| file.flush())
            .and_then(|_| file.sync_all())
            .map_err(|source| cache_io(&temporary, source))?;
        drop(file);
        replace_cache_file(&temporary, path).map_err(|source| cache_io(path, source))
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
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

struct ProcessPrepareGuard {
    key: PathBuf,
}

impl ProcessPrepareGuard {
    fn acquire(key: PathBuf) -> Result<Self, ServerInstallerError> {
        let (active, ready) =
            ACTIVE_PREPARES.get_or_init(|| (Mutex::new(HashSet::new()), Condvar::new()));
        let mut active = active.lock().map_err(|_| ServerInstallerError::AssetSet {
            reason: "server installer in-process cache lock was poisoned".to_string(),
        })?;
        while active.contains(&key) {
            active = ready
                .wait(active)
                .map_err(|_| ServerInstallerError::AssetSet {
                    reason: "server installer in-process cache wait was poisoned".to_string(),
                })?;
        }
        active.insert(key.clone());
        Ok(Self { key })
    }
}

impl Drop for ProcessPrepareGuard {
    fn drop(&mut self) {
        let Some((active, ready)) = ACTIVE_PREPARES.get() else {
            return;
        };
        match active.lock() {
            Ok(mut active) => {
                active.remove(&self.key);
                ready.notify_all();
            }
            Err(_) => {
                eprintln!(
                    "[build] server installer in-process cache lock was poisoned while releasing '{}'",
                    self.key.display()
                );
            }
        }
    }
}

fn validate_release_index(
    bytes: &[u8],
    contract: &ReleaseContract,
) -> Result<Vec<ExpectedAsset>, ServerInstallerError> {
    let index: ReleaseIndex =
        serde_json::from_slice(bytes).map_err(|source| ServerInstallerError::ReleaseIndex {
            reason: source.to_string(),
        })?;
    if index.schema_version != contract.schema_version
        || index.package != contract.package
        || index.version != contract.version
        || index.tag != contract.tag
    {
        return Err(ServerInstallerError::ReleaseIndex {
            reason: "tag, version, package, or schema does not match the selected GitHub release"
                .to_string(),
        });
    }
    if index.commit.len() != 40 || !index.commit.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(ServerInstallerError::ReleaseIndex {
            reason: "commit must contain exactly 40 hexadecimal characters".to_string(),
        });
    }
    if index.metadata.provenance != "provenance.sigstore.json"
        || !valid_workflow_url(&index.metadata.workflow)
    {
        return Err(ServerInstallerError::ReleaseIndex {
            reason: "release metadata does not match the published release".to_string(),
        });
    }
    if index.assets.len() != EXPECTED_ASSET_SPECS.len() {
        return Err(ServerInstallerError::ReleaseIndex {
            reason: format!("expected exactly 3 assets, found {}", index.assets.len()),
        });
    }
    let mut names = HashSet::new();
    let mut expected_assets = Vec::with_capacity(EXPECTED_ASSET_SPECS.len());
    for spec in EXPECTED_ASSET_SPECS {
        let Some(actual) = index.assets.iter().find(|asset| asset.name == spec.name) else {
            return Err(ServerInstallerError::ReleaseIndex {
                reason: format!("missing asset '{}'", spec.name),
            });
        };
        if !names.insert(actual.name.as_str()) {
            return Err(ServerInstallerError::ReleaseIndex {
                reason: format!("duplicate asset '{}'", actual.name),
            });
        }
        let release_asset = contract
            .release_assets
            .iter()
            .find(|asset| asset.name == spec.name)
            .ok_or_else(|| ServerInstallerError::ReleaseIndex {
                reason: format!("GitHub release metadata omitted asset '{}'", spec.name),
            })?;
        validate_release_download_url(&actual.url, &contract.tag, spec.name).map_err(|error| {
            ServerInstallerError::ReleaseIndex {
                reason: error.to_string(),
            }
        })?;
        if actual.version != contract.version
            || actual.platform != spec.platform
            || actual.target != spec.target
            || actual.url != release_asset.browser_url
            || actual.size != release_asset.size
            || actual.size == 0
            || actual.sha256.len() != 64
            || !actual.sha256.bytes().all(|byte| byte.is_ascii_hexdigit())
        {
            return Err(ServerInstallerError::ReleaseIndex {
                reason: format!(
                    "asset '{}' does not match the selected GitHub release and platform contract",
                    spec.name
                ),
            });
        }
        expected_assets.push(ExpectedAsset {
            name: spec.name.to_string(),
            version: actual.version.clone(),
            platform: spec.platform.to_string(),
            target: spec.target.to_string(),
            download_url: release_asset.api_url.clone(),
            browser_url: actual.url.clone(),
            asset_id: release_asset.id,
            size: actual.size,
            sha256: actual.sha256.to_ascii_lowercase(),
            executable: spec.executable,
        });
    }
    if names.len() != index.assets.len() {
        return Err(ServerInstallerError::ReleaseIndex {
            reason: "release index contains duplicate asset names".to_string(),
        });
    }
    Ok(expected_assets)
}

fn validate_direct_release_index(
    bytes: &[u8],
    contract: DirectReleaseContract<'_>,
) -> Result<Vec<ExpectedAsset>, ServerInstallerError> {
    let index: ReleaseIndex =
        serde_json::from_slice(bytes).map_err(|source| ServerInstallerError::ReleaseIndex {
            reason: source.to_string(),
        })?;
    if index.schema_version != 1
        || index.package != RELEASE_PACKAGE
        || index.version != contract.version
        || index.tag != contract.tag
    {
        return Err(ServerInstallerError::ReleaseIndex {
            reason:
                "tag, version, package, or schema does not match the strict direct release contract"
                    .to_string(),
        });
    }
    if index.commit.len() != 40 || !index.commit.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(ServerInstallerError::ReleaseIndex {
            reason: "commit must contain exactly 40 hexadecimal characters".to_string(),
        });
    }
    if index.metadata.provenance != "provenance.sigstore.json"
        || !valid_workflow_url(&index.metadata.workflow)
    {
        return Err(ServerInstallerError::ReleaseIndex {
            reason: "release metadata does not match the published release".to_string(),
        });
    }
    if index.assets.len() > MAX_RELEASE_ASSETS {
        return Err(ServerInstallerError::ReleaseIndex {
            reason: format!(
                "direct release index contains {} assets, exceeding limit {MAX_RELEASE_ASSETS}",
                index.assets.len()
            ),
        });
    }

    let mut names = HashSet::new();
    let mut expected_assets = Vec::with_capacity(EXPECTED_ASSET_SPECS.len());
    for spec in EXPECTED_ASSET_SPECS {
        let Some(actual) = index.assets.iter().find(|asset| asset.name == spec.name) else {
            return Err(ServerInstallerError::ReleaseIndex {
                reason: format!(
                    "missing required installer for platform '{}': asset '{}'",
                    spec.platform, spec.name
                ),
            });
        };
        if !names.insert(actual.name.as_str()) {
            return Err(ServerInstallerError::ReleaseIndex {
                reason: format!("duplicate installer asset '{}'", actual.name),
            });
        }
        validate_release_download_url(&actual.url, contract.tag, spec.name).map_err(|error| {
            ServerInstallerError::ReleaseIndex {
                reason: error.to_string(),
            }
        })?;
        if actual.version != contract.version
            || actual.platform != spec.platform
            || actual.target != spec.target
            || actual.size == 0
            || !valid_sha256(&actual.sha256)
        {
            return Err(ServerInstallerError::ReleaseIndex {
                reason: format!(
                    "installer asset '{}' does not match platform '{}' and direct-release contract",
                    spec.name, spec.platform
                ),
            });
        }
        expected_assets.push(ExpectedAsset {
            name: spec.name.to_string(),
            version: actual.version.clone(),
            platform: spec.platform.to_string(),
            target: spec.target.to_string(),
            download_url: actual.url.clone(),
            browser_url: actual.url.clone(),
            asset_id: 0,
            size: actual.size,
            sha256: actual.sha256.to_ascii_lowercase(),
            executable: spec.executable,
        });
    }
    if index.assets.len() != EXPECTED_ASSET_SPECS.len() || names.len() != index.assets.len() {
        return Err(ServerInstallerError::ReleaseIndex {
            reason: "release index contains duplicate or unexpected installer asset names"
                .to_string(),
        });
    }
    Ok(expected_assets)
}

fn valid_workflow_url(value: &str) -> bool {
    let Ok(url) = Url::parse(value) else {
        return false;
    };
    if url.scheme() != "https"
        || url.host_str() != Some("github.com")
        || url.port().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return false;
    }
    let segments = url
        .path_segments()
        .map(Iterator::collect::<Vec<_>>)
        .unwrap_or_default();
    segments.len() == 7
        && segments[..4] == ["Circulate233", "MCServerDownloadTool", "actions", "runs"]
        && decimal_id(segments[4])
        && segments[5] == "attempts"
        && decimal_id(segments[6])
}

fn valid_sha256(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn decimal_id(value: &str) -> bool {
    !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit())
}

fn cached_file_matches(
    path: &Path,
    expected_size: u64,
    expected_sha256: &str,
) -> Result<bool, ServerInstallerError> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(source) => return Err(cache_io(path, source)),
    };
    if !is_regular_cache_file(&metadata) || metadata.len() != expected_size {
        return Ok(false);
    }
    let mut file = File::open(path).map_err(|source| cache_io(path, source))?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 256 * 1024];
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|source| cache_io(path, source))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(format!("{:x}", hasher.finalize()) == expected_sha256)
}

fn is_regular_cache_file(metadata: &fs::Metadata) -> bool {
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return false;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
        if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return false;
        }
    }
    true
}

fn create_dir_all(path: &Path) -> Result<(), ServerInstallerError> {
    fs::create_dir_all(path).map_err(|source| cache_io(path, source))
}

fn read_file(path: &Path) -> Result<Vec<u8>, ServerInstallerError> {
    fs::read(path).map_err(|source| cache_io(path, source))
}

fn cache_io(path: &Path, source: std::io::Error) -> ServerInstallerError {
    ServerInstallerError::CacheIo {
        path: path.display().to_string(),
        source,
    }
}

fn no_progress_observer() -> Arc<dyn TransferObserver> {
    Arc::new(|_| {})
}

#[cfg(test)]
mod tests {
    use std::io::Write;
    use std::path::PathBuf;
    use std::process::Command;
    use std::sync::Mutex;
    use std::time::{Duration, Instant};

    use super::*;

    const TEST_PLATFORM: ServerPlatform = ServerPlatform::LinuxX86_64;
    use crate::net::ArtifactOutcome;

    struct FixtureTransfer {
        contents: Vec<(String, Vec<u8>)>,
        batches: Mutex<Vec<Vec<String>>>,
    }

    struct CrossProcessFixtureTransfer {
        trace_path: PathBuf,
    }

    struct FixtureSource {
        releases: Mutex<Vec<GitHubRelease>>,
        api_calls: Mutex<usize>,
        batches: Mutex<Vec<(String, Vec<String>)>>,
    }

    struct DirectFallbackSource {
        index: Vec<u8>,
        latest_final_url: Option<String>,
        corrupt_asset: Option<&'static str>,
        fail_asset_downloads: bool,
        transfers: Mutex<Vec<Vec<String>>>,
        metadata_urls: Mutex<Vec<String>>,
    }

    fn direct_fallback_source(index: Vec<u8>) -> DirectFallbackSource {
        DirectFallbackSource {
            index,
            latest_final_url: None,
            corrupt_asset: None,
            fail_asset_downloads: false,
            transfers: Mutex::new(Vec::new()),
            metadata_urls: Mutex::new(Vec::new()),
        }
    }

    impl HttpTransport for DirectFallbackSource {
        fn get_bytes(
            &self,
            request: HttpRequest,
        ) -> Result<crate::net::HttpResponse, NetworkError> {
            let url = request.url().as_str();
            self.metadata_urls.lock().unwrap().push(url.to_string());
            if url.starts_with(RELEASES_API_URL) {
                return Err(NetworkError::HttpStatus {
                    url: request.url().to_string(),
                    status: 403,
                    attempts: 1,
                });
            }
            if url == LATEST_RELEASE_INDEX_URL {
                return Ok(crate::net::HttpResponse {
                    final_url: self
                        .latest_final_url
                        .as_deref()
                        .unwrap_or(url)
                        .parse()
                        .unwrap(),
                    status: reqwest::StatusCode::OK,
                    headers: reqwest::header::HeaderMap::new(),
                    body: self.index.clone(),
                });
            }
            panic!("unexpected direct fallback metadata request '{url}'");
        }
    }

    impl ArtifactTransfer for DirectFallbackSource {
        fn transfer_many(
            &self,
            requests: Vec<ArtifactRequest>,
            _observer: Arc<dyn TransferObserver>,
        ) -> Vec<Result<ArtifactOutcome, TransferError>> {
            self.transfers.lock().unwrap().push(
                requests
                    .iter()
                    .map(|request| request.task_id().to_string())
                    .collect(),
            );
            requests
                .into_iter()
                .map(|request| {
                    if self.fail_asset_downloads {
                        return Err(TransferError::Network {
                            task_id: request.task_id().to_string(),
                            source: NetworkError::Request {
                                url: github_asset_url("v1.0.0", request.task_id()),
                                attempts: 1,
                                reason: "fixture asset transfer disabled".to_string(),
                            },
                        });
                    }
                    let bytes = if self.corrupt_asset == Some(request.task_id()) {
                        b"X".as_slice()
                    } else {
                        fixture_asset_bytes(request.task_id())
                    };
                    if let Some(parent) = request.target().parent() {
                        fs::create_dir_all(parent).unwrap();
                    }
                    fs::write(request.target(), bytes).unwrap();
                    Ok(ArtifactOutcome {
                        task_id: request.task_id().to_string(),
                        source_url: github_asset_url("v1.0.0", request.task_id())
                            .parse()
                            .unwrap(),
                        target: request.target().to_path_buf(),
                        bytes: bytes.len() as u64,
                        mode: crate::net::DownloadMode::Single,
                    })
                })
                .collect()
        }
    }

    impl HttpTransport for FixtureSource {
        fn get_bytes(
            &self,
            request: HttpRequest,
        ) -> Result<crate::net::HttpResponse, NetworkError> {
            *self.api_calls.lock().unwrap() += 1;
            Ok(crate::net::HttpResponse {
                final_url: request.url().clone(),
                status: reqwest::StatusCode::OK,
                headers: reqwest::header::HeaderMap::new(),
                body: serde_json::to_vec(&*self.releases.lock().unwrap()).unwrap(),
            })
        }
    }

    impl ArtifactTransfer for FixtureSource {
        fn transfer_many(
            &self,
            requests: Vec<ArtifactRequest>,
            _observer: Arc<dyn TransferObserver>,
        ) -> Vec<Result<ArtifactOutcome, TransferError>> {
            let tag = requests
                .first()
                .and_then(|request| request.target().parent())
                .and_then(Path::file_name)
                .and_then(|name| name.to_str())
                .unwrap()
                .to_string();
            self.batches.lock().unwrap().push((
                tag.clone(),
                requests
                    .iter()
                    .map(|request| request.task_id().to_string())
                    .collect(),
            ));
            requests
                .into_iter()
                .map(|request| {
                    let index = release_index(&tag);
                    let bytes = if request.task_id() == "server-installer-release-index" {
                        index.as_slice()
                    } else {
                        fixture_asset_bytes(request.task_id())
                    };
                    if let Some(parent) = request.target().parent() {
                        fs::create_dir_all(parent).unwrap();
                    }
                    fs::write(request.target(), bytes).unwrap();
                    Ok(ArtifactOutcome {
                        task_id: request.task_id().to_string(),
                        source_url: github_asset_url(&tag, request.task_id()).parse().unwrap(),
                        target: request.target().to_path_buf(),
                        bytes: bytes.len() as u64,
                        mode: crate::net::DownloadMode::Single,
                    })
                })
                .collect()
        }
    }

    impl ArtifactTransfer for CrossProcessFixtureTransfer {
        fn transfer_many(
            &self,
            requests: Vec<ArtifactRequest>,
            _observer: Arc<dyn TransferObserver>,
        ) -> Vec<Result<ArtifactOutcome, TransferError>> {
            std::thread::sleep(Duration::from_millis(300));
            requests
                .into_iter()
                .map(|request| {
                    let index = release_index("v1.0.0");
                    let bytes: &[u8] = match request.task_id() {
                        "server-installer-release-index" => &index,
                        "MCServerDownloadTool-linux-x86_64" => b"L",
                        "MCServerDownloadTool-macos-aarch64" => b"M",
                        "MCServerDownloadTool-windows-x86_64.exe" => b"W",
                        other => panic!("unexpected cross-process fixture task '{other}'"),
                    };
                    if let Some(parent) = request.target().parent() {
                        fs::create_dir_all(parent).unwrap();
                    }
                    fs::write(request.target(), bytes).unwrap();
                    let mut trace = OpenOptions::new()
                        .create(true)
                        .append(true)
                        .open(&self.trace_path)
                        .unwrap();
                    writeln!(trace, "{}", request.task_id()).unwrap();
                    trace.flush().unwrap();
                    Ok(ArtifactOutcome {
                        task_id: request.task_id().to_string(),
                        source_url: "https://example.invalid/fixture".parse().unwrap(),
                        target: request.target().to_path_buf(),
                        bytes: bytes.len() as u64,
                        mode: crate::net::DownloadMode::Single,
                    })
                })
                .collect()
        }
    }

    impl ArtifactTransfer for FixtureTransfer {
        fn transfer_many(
            &self,
            requests: Vec<ArtifactRequest>,
            _observer: Arc<dyn TransferObserver>,
        ) -> Vec<Result<ArtifactOutcome, TransferError>> {
            self.batches.lock().unwrap().push(
                requests
                    .iter()
                    .map(|request| request.task_id().to_string())
                    .collect(),
            );
            requests
                .into_iter()
                .map(|request| {
                    let bytes = self
                        .contents
                        .iter()
                        .find(|(name, _)| name == request.task_id())
                        .unwrap()
                        .1
                        .clone();
                    if let Some(parent) = request.target().parent() {
                        fs::create_dir_all(parent).unwrap();
                    }
                    let mut file = File::create(request.target()).unwrap();
                    file.write_all(&bytes).unwrap();
                    Ok(ArtifactOutcome {
                        task_id: request.task_id().to_string(),
                        source_url: "https://example.invalid/fixture".parse().unwrap(),
                        target: request.target().to_path_buf(),
                        bytes: bytes.len() as u64,
                        mode: crate::net::DownloadMode::Single,
                    })
                })
                .collect()
        }
    }

    fn github_asset_url(tag: &str, name: &str) -> String {
        format!("{RELEASE_DOWNLOAD_PREFIX}/{tag}/{name}")
    }

    fn github_asset_api_url(id: u64) -> String {
        format!("{RELEASE_ASSET_API_PREFIX}/{id}")
    }

    fn fixture_asset_bytes(name: &str) -> &'static [u8] {
        match name {
            "MCServerDownloadTool-linux-x86_64" => b"L",
            "MCServerDownloadTool-macos-aarch64" => b"M",
            "MCServerDownloadTool-windows-x86_64.exe" => b"W",
            other => panic!("unexpected fixture asset '{other}'"),
        }
    }

    fn release_index(tag: &str) -> Vec<u8> {
        let version = tag.strip_prefix('v').unwrap();
        let assets = EXPECTED_ASSET_SPECS
            .iter()
            .map(|spec| {
                let bytes = fixture_asset_bytes(spec.name);
                serde_json::json!({
                    "name": spec.name,
                    "version": version,
                    "platform": spec.platform,
                    "target": spec.target,
                    "size": bytes.len(),
                    "sha256": format!("{:x}", Sha256::digest(bytes)),
                    "url": github_asset_url(tag, spec.name),
                })
            })
            .collect::<Vec<_>>();
        serde_json::to_vec(&serde_json::json!({
            "schemaVersion": 1,
            "package": RELEASE_PACKAGE,
            "version": version,
            "tag": tag,
            "commit": "0123456789abcdef0123456789abcdef01234567",
            "assets": assets,
            "metadata": {
                "provenance": "provenance.sigstore.json",
                "workflow": "https://github.com/Circulate233/MCServerDownloadTool/actions/runs/30379199744/attempts/1"
            }
        }))
        .unwrap()
    }

    fn fixture_release(tag: &str, draft: bool, prerelease: bool) -> GitHubRelease {
        let index = release_index(tag);
        let mut assets = vec![GitHubReleaseAsset {
            id: 100,
            url: github_asset_api_url(100),
            name: "release-index.json".to_string(),
            size: index.len() as u64,
            browser_download_url: github_asset_url(tag, "release-index.json"),
        }];
        assets.extend(
            EXPECTED_ASSET_SPECS
                .iter()
                .enumerate()
                .map(|(offset, spec)| GitHubReleaseAsset {
                    id: 101 + offset as u64,
                    url: github_asset_api_url(101 + offset as u64),
                    name: spec.name.to_string(),
                    size: fixture_asset_bytes(spec.name).len() as u64,
                    browser_download_url: github_asset_url(tag, spec.name),
                }),
        );
        GitHubRelease {
            tag_name: tag.to_string(),
            draft,
            prerelease,
            assets,
        }
    }

    fn fixture_contract(tag: &str) -> ReleaseContract {
        release_contract(&fixture_release(tag, false, false)).unwrap()
    }

    fn fixture_contents(tag: &str) -> Vec<(String, Vec<u8>)> {
        let mut contents = vec![(
            "server-installer-release-index".to_string(),
            release_index(tag),
        )];
        contents.extend(EXPECTED_ASSET_SPECS.iter().map(|spec| {
            (
                spec.name.to_string(),
                fixture_asset_bytes(spec.name).to_vec(),
            )
        }));
        contents
    }

    #[test]
    fn stable_release_selection_uses_semver_and_rejects_nonstable_candidates() {
        let mut invalid_tag = fixture_release("v0.0.1", false, false);
        invalid_tag.tag_name = "not-a-version".to_string();
        let releases = vec![
            fixture_release("v1.9.0", false, false),
            fixture_release("v1.10.0-rc.1", false, true),
            invalid_tag,
            fixture_release("v9.0.0", true, false),
            fixture_release("v1.10.0", false, false),
        ];
        let selected = select_latest_stable_release(&releases).unwrap();
        assert_eq!(selected.tag_name, "v1.10.0");

        let mut missing_prefix = fixture_release("v0.0.1", false, false);
        missing_prefix.tag_name = "2.0.0".to_string();
        let no_stable = vec![
            fixture_release("v2.0.0-rc.1", false, true),
            fixture_release("v2.0.0", true, false),
            missing_prefix,
        ];
        assert!(select_latest_stable_release(&no_stable).is_err());
    }

    #[test]
    fn release_metadata_and_index_are_bound_to_the_selected_tag() {
        let mut wrong_url = fixture_release("v1.10.0", false, false);
        wrong_url.assets[1].browser_download_url =
            github_asset_url("v1.10.0", "../MCServerDownloadTool-linux-x86_64");
        assert!(release_contract(&wrong_url).is_err());

        let mut wrong_asset_api = fixture_release("v1.10.0", false, false);
        wrong_asset_api.assets[1].url =
            "https://api.github.com/repos/Circulate233/MCServerDownloadTool/releases/assets/999"
                .to_string();
        assert!(release_contract(&wrong_asset_api).is_err());

        let contract = fixture_contract("v1.10.0");
        validate_release_index(&release_index("v1.10.0"), &contract).unwrap();

        let mut mismatched: serde_json::Value =
            serde_json::from_slice(&release_index("v1.10.0")).unwrap();
        mismatched["tag"] = serde_json::json!("v1.9.0");
        assert!(
            validate_release_index(&serde_json::to_vec(&mismatched).unwrap(), &contract).is_err()
        );

        let mut wrong_repository: serde_json::Value =
            serde_json::from_slice(&release_index("v1.10.0")).unwrap();
        wrong_repository["assets"][0]["url"] = serde_json::json!(
            "https://github.com/other/MCServerDownloadTool/releases/download/v1.10.0/MCServerDownloadTool-linux-x86_64"
        );
        assert!(
            validate_release_index(&serde_json::to_vec(&wrong_repository).unwrap(), &contract)
                .is_err()
        );

        let mut unknown: serde_json::Value =
            serde_json::from_slice(&release_index("v1.10.0")).unwrap();
        unknown["assets"][0]["unexpected"] = serde_json::json!(true);
        assert!(validate_release_index(&serde_json::to_vec(&unknown).unwrap(), &contract).is_err());

        assert_eq!(
            contract.release_assets[0].api_url,
            github_asset_api_url(contract.release_assets[0].id)
        );
        assert_eq!(
            contract.index.api_url,
            github_asset_api_url(contract.index.id)
        );
    }

    #[test]
    fn github_api_rate_limit_is_classified_without_exposing_unbounded_response_data() {
        let error = ServerInstallerError::Request(NetworkError::HttpStatus {
            url: format!("{RELEASES_API_URL}?per_page=100&page=1&token=api-secret"),
            status: 403,
            attempts: 1,
        });
        assert!(is_recoverable_releases_api_failure(&error));
        let summary = releases_api_failure_summary(&error);
        assert!(summary.contains("HTTP 403"));
        assert!(summary.contains("rate limit or access policy"));
        assert!(summary.len() < 512);
        assert!(!summary.contains("api-secret"));
        assert!(!summary.contains("?per_page"));
    }

    #[test]
    fn api_rate_limit_validates_full_direct_release_metadata_but_downloads_one_platform_asset() {
        let project = tempfile::tempdir().unwrap();
        let source = HttpServerInstallerSource {
            transfer: direct_fallback_source(release_index("v1.0.0")),
        };

        let asset = source.prepare(project.path(), TEST_PLATFORM).unwrap();
        assert_eq!(asset.platform, TEST_PLATFORM);
        assert_eq!(asset.name, "MCServerDownloadTool-linux-x86_64");
        assert!(cached_file_matches(&asset.path, asset.size, &asset.sha256).unwrap());
        let transfers = source.transfer.transfers.lock().unwrap();
        assert_eq!(
            transfers.as_slice(),
            &[vec!["MCServerDownloadTool-linux-x86_64".to_string()]]
        );
        let metadata_urls = source.transfer.metadata_urls.lock().unwrap();
        assert_eq!(metadata_urls.len(), 2);
        assert!(metadata_urls[0].starts_with(RELEASES_API_URL));
        assert_eq!(metadata_urls[1], LATEST_RELEASE_INDEX_URL);
    }

    #[test]
    fn api_rate_limit_reuses_verified_direct_asset_cache_without_downloading_assets_again() {
        let project = tempfile::tempdir().unwrap();
        let source = HttpServerInstallerSource {
            transfer: direct_fallback_source(release_index("v1.0.0")),
        };

        source.prepare(project.path(), TEST_PLATFORM).unwrap();
        source.prepare(project.path(), TEST_PLATFORM).unwrap();

        let transfers = source.transfer.transfers.lock().unwrap();
        assert_eq!(transfers.len(), 1);
        assert_eq!(transfers[0], vec!["MCServerDownloadTool-linux-x86_64"]);
        let metadata_urls = source.transfer.metadata_urls.lock().unwrap();
        assert_eq!(metadata_urls.len(), 4);
        assert!(metadata_urls[0].starts_with(RELEASES_API_URL));
        assert_eq!(metadata_urls[1], LATEST_RELEASE_INDEX_URL);
        assert!(metadata_urls[2].starts_with(RELEASES_API_URL));
        assert_eq!(metadata_urls[3], LATEST_RELEASE_INDEX_URL);
    }

    #[test]
    fn direct_asset_hash_mismatch_is_rejected_after_transfer() {
        let source = HttpServerInstallerSource {
            transfer: DirectFallbackSource {
                corrupt_asset: Some("MCServerDownloadTool-linux-x86_64"),
                ..direct_fallback_source(release_index("v1.0.0"))
            },
        };

        let error = source
            .prepare(tempfile::tempdir().unwrap().path(), TEST_PLATFORM)
            .unwrap_err();
        assert!(error.to_string().contains("SHA-256 verification"));
    }

    #[test]
    fn direct_fallback_reuses_requested_asset_without_other_platform_assets() {
        let project = tempfile::tempdir().unwrap();
        let cache = project.path().join(".mcmpu/cache/server-installer/v1.0.0");
        fs::create_dir_all(&cache).unwrap();
        let index = release_index("v1.0.0");
        write_cache_bytes(&cache.join("release-index.json"), &index).unwrap();
        let contract = DirectReleaseContract {
            tag: "v1.0.0",
            version: "1.0.0",
        };
        write_direct_release_index_state(&cache, contract, &index).unwrap();
        fs::write(
            cache.join("MCServerDownloadTool-linux-x86_64"),
            fixture_asset_bytes("MCServerDownloadTool-linux-x86_64"),
        )
        .unwrap();
        let source = HttpServerInstallerSource {
            transfer: DirectFallbackSource {
                fail_asset_downloads: true,
                ..direct_fallback_source(index)
            },
        };

        let asset = source.prepare(project.path(), TEST_PLATFORM).unwrap();
        assert_eq!(asset.platform, TEST_PLATFORM);
        assert_eq!(asset.name, "MCServerDownloadTool-linux-x86_64");
        let transfers = source.transfer.transfers.lock().unwrap();
        assert!(transfers.is_empty());
    }

    #[test]
    fn direct_release_index_reports_the_missing_platform_installer() {
        let mut index: serde_json::Value =
            serde_json::from_slice(&release_index("v1.0.0")).unwrap();
        index["assets"] = serde_json::Value::Array(
            index["assets"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|asset| asset["platform"] != "linux-x86_64")
                .cloned()
                .collect(),
        );
        let source = HttpServerInstallerSource {
            transfer: direct_fallback_source(serde_json::to_vec(&index).unwrap()),
        };

        let error = source
            .prepare(tempfile::tempdir().unwrap().path(), TEST_PLATFORM)
            .unwrap_err();
        assert!(error.to_string().contains("platform 'linux-x86_64'"));
        assert!(source.transfer.transfers.lock().unwrap().is_empty());
    }

    #[test]
    fn direct_release_index_rejects_a_tag_mismatched_asset_path() {
        let mut index: serde_json::Value =
            serde_json::from_slice(&release_index("v1.0.0")).unwrap();
        index["assets"][0]["url"] = serde_json::json!(github_asset_url(
            "v1.0.1",
            "MCServerDownloadTool-linux-x86_64"
        ));
        let source = HttpServerInstallerSource {
            transfer: direct_fallback_source(serde_json::to_vec(&index).unwrap()),
        };

        let error = source
            .prepare(tempfile::tempdir().unwrap().path(), TEST_PLATFORM)
            .unwrap_err();
        assert!(error.to_string().contains("URL must be"));
        assert!(source.transfer.transfers.lock().unwrap().is_empty());
    }

    #[test]
    fn latest_direct_release_index_requires_a_stable_matching_tag() {
        assert_eq!(
            latest_direct_release_tag(&release_index("v1.0.0")).unwrap(),
            "v1.0.0"
        );
        let mut prerelease: serde_json::Value =
            serde_json::from_slice(&release_index("v1.0.0")).unwrap();
        prerelease["tag"] = serde_json::json!("v1.0.0-rc.1");
        prerelease["version"] = serde_json::json!("1.0.0-rc.1");
        assert!(latest_direct_release_tag(&serde_json::to_vec(&prerelease).unwrap()).is_err());

        let mut mismatch: serde_json::Value =
            serde_json::from_slice(&release_index("v1.0.0")).unwrap();
        mismatch["version"] = serde_json::json!("1.0.1");
        assert!(latest_direct_release_tag(&serde_json::to_vec(&mismatch).unwrap()).is_err());
    }

    #[test]
    fn latest_direct_release_rejects_a_final_tag_url_that_disagrees_with_the_index() {
        let source = HttpServerInstallerSource {
            transfer: DirectFallbackSource {
                latest_final_url: Some(github_asset_url("v1.0.1", "release-index.json")),
                ..direct_fallback_source(release_index("v1.0.0"))
            },
        };

        let error = source
            .prepare(tempfile::tempdir().unwrap().path(), TEST_PLATFORM)
            .unwrap_err();
        assert!(error.to_string().contains("untrusted origin"));
        assert!(source.transfer.transfers.lock().unwrap().is_empty());
    }

    #[test]
    fn latest_direct_release_accepts_the_github_asset_cdn_as_a_non_authoritative_redirect() {
        let source = HttpServerInstallerSource {
            transfer: DirectFallbackSource {
                latest_final_url: Some(
                    "https://release-assets.githubusercontent.com/github-production-release-asset/1315074018/fixture"
                        .to_string(),
                ),
                ..direct_fallback_source(release_index("v1.0.0"))
            },
        };

        let asset = source
            .prepare(tempfile::tempdir().unwrap().path(), TEST_PLATFORM)
            .unwrap();
        assert_eq!(asset.platform, TEST_PLATFORM);
    }

    #[test]
    fn cache_reuses_valid_files_and_redownloads_only_corruption() {
        let contract = fixture_contract("v1.10.0");
        let transfer = FixtureTransfer {
            contents: fixture_contents("v1.10.0"),
            batches: Mutex::new(Vec::new()),
        };
        let temp = tempfile::tempdir().unwrap();
        let cache = temp.path().join(".mcmpu/cache/server-installer/v1.10.0");
        fs::create_dir_all(&cache).unwrap();
        fs::write(cache.join(".prepare.lock"), b"lock identity").unwrap();

        prepare_release(&transfer, temp.path(), &contract, TEST_PLATFORM).unwrap();
        prepare_release(&transfer, temp.path(), &contract, TEST_PLATFORM).unwrap();
        fs::write(
            temp.path()
                .join(".mcmpu/cache/server-installer/v1.10.0/MCServerDownloadTool-linux-x86_64"),
            b"X",
        )
        .unwrap();
        prepare_release(&transfer, temp.path(), &contract, TEST_PLATFORM).unwrap();

        let batches = transfer.batches.lock().unwrap();
        assert_eq!(batches[0], vec!["server-installer-release-index"]);
        assert_eq!(batches[1], vec!["MCServerDownloadTool-linux-x86_64"]);
        assert_eq!(batches[2], vec!["MCServerDownloadTool-linux-x86_64"]);
        assert_eq!(batches.len(), 3);
        assert_eq!(
            fs::read(cache.join(".prepare.lock")).unwrap(),
            b"lock identity"
        );
    }

    #[test]
    fn concurrent_prepare_waits_and_reuses_one_verified_cache_fill() {
        let contract = fixture_contract("v1.10.0");
        let transfer = Arc::new(FixtureTransfer {
            contents: fixture_contents("v1.10.0"),
            batches: Mutex::new(Vec::new()),
        });
        let project = Arc::new(tempfile::tempdir().unwrap());
        let first_transfer = Arc::clone(&transfer);
        let first_project = Arc::clone(&project);
        let first_contract = contract.clone();
        let first = std::thread::spawn(move || {
            prepare_release(
                first_transfer.as_ref(),
                first_project.path(),
                &first_contract,
                TEST_PLATFORM,
            )
            .unwrap()
        });
        let second_transfer = Arc::clone(&transfer);
        let second_project = Arc::clone(&project);
        let second = std::thread::spawn(move || {
            prepare_release(
                second_transfer.as_ref(),
                second_project.path(),
                &contract,
                TEST_PLATFORM,
            )
            .unwrap()
        });

        let first = first.join().unwrap();
        let second = second.join().unwrap();
        assert_eq!(first.path, second.path);
        assert_eq!(first.size, 1);
        let batches = transfer.batches.lock().unwrap();
        assert_eq!(batches.len(), 2);
        assert_eq!(batches[0], vec!["server-installer-release-index"]);
        assert_eq!(batches[1], vec!["MCServerDownloadTool-linux-x86_64"]);
    }

    #[test]
    fn a_new_latest_release_uses_a_tag_isolated_cache() {
        let source = HttpServerInstallerSource {
            transfer: FixtureSource {
                releases: Mutex::new(vec![fixture_release("v1.9.0", false, false)]),
                api_calls: Mutex::new(0),
                batches: Mutex::new(Vec::new()),
            },
        };
        let project = tempfile::tempdir().unwrap();

        let first = source.prepare(project.path(), TEST_PLATFORM).unwrap();
        assert!(
            first
                .path
                .components()
                .any(|component| component.as_os_str() == "v1.9.0")
        );

        *source.transfer.releases.lock().unwrap() = vec![fixture_release("v1.10.0", false, false)];
        let second = source.prepare(project.path(), TEST_PLATFORM).unwrap();
        assert!(
            second
                .path
                .components()
                .any(|component| component.as_os_str() == "v1.10.0")
        );
        assert_eq!(*source.transfer.api_calls.lock().unwrap(), 2);

        let cache_root = project.path().join(".mcmpu/cache/server-installer");
        assert!(cache_root.join("v1.9.0/.release-state.json").is_file());
        assert!(cache_root.join("v1.10.0/.release-state.json").is_file());
        let batches = source.transfer.batches.lock().unwrap();
        assert_eq!(
            batches
                .iter()
                .map(|(tag, _)| tag.as_str())
                .collect::<Vec<_>>(),
            ["v1.9.0", "v1.9.0", "v1.10.0", "v1.10.0"]
        );
    }

    #[test]
    fn cross_process_prepare_waits_then_reuses_without_duplicate_downloads() {
        let project = tempfile::tempdir().unwrap();
        let contract = fixture_contract("v1.0.0");
        let index = release_index("v1.0.0");
        let expected_assets = validate_release_index(&index, &contract).unwrap();
        assert_eq!(expected_assets.len(), EXPECTED_ASSET_SPECS.len());
        for spec in EXPECTED_ASSET_SPECS {
            assert!(expected_assets.iter().any(|asset| asset.name == spec.name));
        }
        let barrier_dir = project.path().join("barrier");
        fs::create_dir(&barrier_dir).unwrap();
        let executable = std::env::current_exe().unwrap();
        let spawn_child = || {
            Command::new(&executable)
                .args([
                    "--ignored",
                    "--exact",
                    "server::installer::tests::installer_cache_process_helper",
                    "--nocapture",
                    "--test-threads=1",
                ])
                .env("MCMPU_INSTALLER_LOCK_TEST_ROOT", project.path())
                .spawn()
                .unwrap()
        };
        let mut first = spawn_child();
        let mut second = spawn_child();

        let first_status = first.wait().unwrap();
        let second_status = second.wait().unwrap();
        assert!(first_status.success(), "first cache process failed");
        assert!(second_status.success(), "second cache process failed");

        let trace = fs::read_to_string(project.path().join("transfer.trace")).unwrap();
        let tasks = trace.lines().collect::<Vec<_>>();
        assert_eq!(
            tasks,
            [
                "server-installer-release-index",
                "MCServerDownloadTool-linux-x86_64",
            ],
            "the waiting process repeated a cache download"
        );
        let cache = project.path().join(".mcmpu/cache/server-installer/v1.0.0");
        let index = release_index("v1.0.0");
        assert!(
            cached_file_matches(
                &cache.join("release-index.json"),
                index.len() as u64,
                &format!("{:x}", Sha256::digest(&index))
            )
            .unwrap()
        );
        assert!(
            cached_file_matches(
                &cache.join("MCServerDownloadTool-linux-x86_64"),
                1,
                &format!("{:x}", Sha256::digest(b"L"))
            )
            .unwrap()
        );
    }

    #[test]
    #[ignore = "spawned explicitly by cross_process_prepare_waits_then_reuses_without_duplicate_downloads"]
    fn installer_cache_process_helper() {
        let Some(project) = std::env::var_os("MCMPU_INSTALLER_LOCK_TEST_ROOT") else {
            return;
        };
        let project = PathBuf::from(project);
        let barrier = project.join("barrier");
        fs::write(barrier.join(std::process::id().to_string()), b"ready").unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let ready = fs::read_dir(&barrier).unwrap().count();
            if ready >= 2 {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for the second cache process"
            );
            std::thread::sleep(Duration::from_millis(10));
        }

        let contract = fixture_contract("v1.0.0");
        let prepared = prepare_release(
            &CrossProcessFixtureTransfer {
                trace_path: project.join("transfer.trace"),
            },
            &project,
            &contract,
            TEST_PLATFORM,
        )
        .unwrap();
        assert_eq!(prepared.platform, TEST_PLATFORM);
        assert_eq!(prepared.size, 1);
    }

    #[test]
    fn server_platform_target_resolver_accepts_supported_targets_and_rejects_unknown_ones() {
        assert_eq!(
            ServerPlatform::from_target_parts("windows", "x86_64").unwrap(),
            ServerPlatform::WindowsX86_64
        );
        assert_eq!(
            ServerPlatform::from_target_parts("linux", "x86_64").unwrap(),
            ServerPlatform::LinuxX86_64
        );
        assert_eq!(
            ServerPlatform::from_target_parts("macos", "aarch64").unwrap(),
            ServerPlatform::MacosAarch64
        );
        assert_eq!(
            ServerPlatform::from_target_parts("linux", "aarch64").unwrap_err(),
            ServerPlatformError {
                os: "linux".to_string(),
                arch: "aarch64".to_string(),
            }
        );
    }
}
