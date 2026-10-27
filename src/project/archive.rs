use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use sha2::{Digest, Sha256};
use thiserror::Error;
use walkdir::WalkDir;
use zip::write::SimpleFileOptions;

use crate::path_safety::{portable_path_identity, validate_portable_relative_path};
use crate::zip::optimize::{minify_json_bytes, optimize_png_bytes};

pub(crate) const DEFAULT_LIMITS: ArchiveLimits = ArchiveLimits {
    max_entries: 100_000,
    max_file_size: 8 * 1024 * 1024 * 1024,
    max_total_uncompressed: 16 * 1024 * 1024 * 1024,
};

const STREAM_BUFFER_SIZE: usize = 256 * 1024;
const MAX_IN_MEMORY_OPTIMIZATION_SIZE: u64 = 128 * 1024 * 1024;
static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, Copy)]
pub(crate) struct ArchiveLimits {
    pub max_entries: usize,
    pub max_file_size: u64,
    pub max_total_uncompressed: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Compression {
    Deflated,
    Stored,
}

#[derive(Debug)]
pub(crate) enum EntrySource {
    Bytes(Vec<u8>),
    File {
        path: PathBuf,
        size: u64,
        expected_sha256: Option<String>,
    },
}

#[derive(Debug)]
pub(crate) struct ArchiveEntry {
    pub name: String,
    pub source: EntrySource,
    pub compression: Compression,
    pub unix_mode: Option<u32>,
    origin: String,
}

impl ArchiveEntry {
    pub(crate) fn bytes(
        name: impl Into<String>,
        bytes: Vec<u8>,
        compression: Compression,
        origin: impl Into<String>,
    ) -> Self {
        Self {
            name: name.into(),
            source: EntrySource::Bytes(bytes),
            compression,
            unix_mode: None,
            origin: origin.into(),
        }
    }

    pub(crate) fn file(
        name: impl Into<String>,
        path: PathBuf,
        size: u64,
        compression: Compression,
        origin: impl Into<String>,
    ) -> Self {
        Self {
            name: name.into(),
            source: EntrySource::File {
                path,
                size,
                expected_sha256: None,
            },
            compression,
            unix_mode: None,
            origin: origin.into(),
        }
    }

    pub(crate) fn verified_file(
        name: impl Into<String>,
        path: PathBuf,
        size: u64,
        sha256: String,
        unix_executable: bool,
        origin: impl Into<String>,
    ) -> Self {
        Self {
            name: name.into(),
            source: EntrySource::File {
                path,
                size,
                expected_sha256: Some(sha256),
            },
            compression: Compression::Stored,
            unix_mode: unix_executable.then_some(0o755),
            origin: origin.into(),
        }
    }

    fn size(&self) -> u64 {
        match &self.source {
            EntrySource::Bytes(bytes) => bytes.len() as u64,
            EntrySource::File { size, .. } => *size,
        }
    }
}

#[derive(Debug)]
pub(crate) struct ArchivePlan {
    entries: BTreeMap<String, ArchiveEntry>,
    total_uncompressed: u64,
    limits: ArchiveLimits,
}

impl ArchivePlan {
    pub(crate) fn new(limits: ArchiveLimits) -> Self {
        Self {
            entries: BTreeMap::new(),
            total_uncompressed: 0,
            limits,
        }
    }

    pub(crate) fn insert(&mut self, entry: ArchiveEntry) -> Result<(), ArchiveError> {
        self.insert_internal(entry, None)
    }

    pub(crate) fn insert_layer(
        &mut self,
        entry: ArchiveEntry,
        replace_origin: Option<&str>,
    ) -> Result<(), ArchiveError> {
        self.insert_internal(entry, replace_origin)
    }

    fn insert_internal(
        &mut self,
        entry: ArchiveEntry,
        replace_origin: Option<&str>,
    ) -> Result<(), ArchiveError> {
        validate_portable_relative_path(&entry.name).map_err(|reason| {
            ArchiveError::UnsafePath {
                path: entry.name.clone(),
                reason: reason.to_string(),
            }
        })?;
        let size = entry.size();
        if size > self.limits.max_file_size {
            return Err(ArchiveError::FileLimit {
                path: entry.name,
                size,
                limit: self.limits.max_file_size,
            });
        }
        let identity =
            portable_path_identity(&entry.name).map_err(|reason| ArchiveError::UnsafePath {
                path: entry.name.clone(),
                reason: reason.to_string(),
            })?;
        let replaced_size = if let Some(existing) = self.entries.get(&identity) {
            if replace_origin != Some(existing.origin.as_str()) {
                return Err(ArchiveError::Conflict {
                    path: entry.name,
                    first: existing.origin.clone(),
                    second: entry.origin,
                });
            }
            existing.size()
        } else {
            if self.entries.len() >= self.limits.max_entries {
                return Err(ArchiveError::EntryLimit {
                    limit: self.limits.max_entries,
                });
            }
            0
        };
        let total = self
            .total_uncompressed
            .checked_sub(replaced_size)
            .and_then(|value| value.checked_add(size))
            .ok_or(ArchiveError::TotalLimit {
                size: u64::MAX,
                limit: self.limits.max_total_uncompressed,
            })?;
        if total > self.limits.max_total_uncompressed {
            return Err(ArchiveError::TotalLimit {
                size: total,
                limit: self.limits.max_total_uncompressed,
            });
        }
        self.entries.insert(identity, entry);
        self.total_uncompressed = total;
        Ok(())
    }

    pub(crate) fn contains(&self, name: &str) -> Result<bool, ArchiveError> {
        let identity = portable_path_identity(name).map_err(|reason| ArchiveError::UnsafePath {
            path: name.to_string(),
            reason: reason.to_string(),
        })?;
        Ok(self.entries.contains_key(&identity))
    }

    pub(crate) fn write_to(&self, output: &Path) -> Result<(), ArchiveError> {
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(output)
            .map_err(|source| io_error(output, source))?;
        let mut zip = zip::ZipWriter::new(file);
        for entry in self.entries.values() {
            write_entry(&mut zip, entry, output)?;
        }
        let mut file = zip.finish().map_err(|source| ArchiveError::Zip {
            path: output.display().to_string(),
            source,
        })?;
        file.flush().map_err(|source| io_error(output, source))?;
        file.sync_all().map_err(|source| io_error(output, source))?;
        Ok(())
    }
}

pub(crate) fn collect_layer(
    plan: &mut ArchivePlan,
    root: &Path,
    archive_prefix: Option<&str>,
    origin: &'static str,
    replace_origin: Option<&str>,
    reject_reserved: bool,
) -> Result<(), ArchiveError> {
    let root_metadata = match fs::symlink_metadata(root) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(source) => return Err(io_error(root, source)),
    };
    reject_special(root, &root_metadata)?;
    if !root_metadata.is_dir() {
        return Err(ArchiveError::NonDirectory {
            path: root.display().to_string(),
        });
    }

    for walked in WalkDir::new(root).follow_links(false) {
        let walked = walked.map_err(|source| ArchiveError::Walk {
            path: root.display().to_string(),
            source,
        })?;
        if walked.path() == root {
            continue;
        }
        let metadata = fs::symlink_metadata(walked.path())
            .map_err(|source| io_error(walked.path(), source))?;
        reject_special(walked.path(), &metadata)?;
        if metadata.is_dir() {
            continue;
        }
        if !metadata.is_file() {
            return Err(ArchiveError::NonRegular {
                path: walked.path().display().to_string(),
            });
        }
        let relative = walked
            .path()
            .strip_prefix(root)
            .map_err(|_| ArchiveError::EscapedRoot {
                path: walked.path().display().to_string(),
            })?;
        let relative = slash_path(relative)?;
        let name = archive_prefix
            .map(|prefix| format!("{prefix}/{relative}"))
            .unwrap_or(relative);
        if reject_reserved {
            reject_reserved_override(&name, walked.path())?;
        }
        let entry = optimized_or_streamed_entry(name, walked.path(), metadata.len(), origin)?;
        plan.insert_layer(entry, replace_origin)?;
    }
    Ok(())
}

pub(crate) fn collect_root_docs(
    plan: &mut ArchivePlan,
    project_dir: &Path,
) -> Result<(), ArchiveError> {
    let entries = fs::read_dir(project_dir).map_err(|source| io_error(project_dir, source))?;
    for entry in entries {
        let entry = entry.map_err(|source| io_error(project_dir, source))?;
        let name = entry.file_name().to_string_lossy().to_string();
        let upper = name.to_ascii_uppercase();
        if !["README", "LICENSE", "NOTICE", "CHANGELOG"]
            .iter()
            .any(|prefix| upper.starts_with(prefix))
        {
            continue;
        }
        let metadata =
            fs::symlink_metadata(entry.path()).map_err(|source| io_error(&entry.path(), source))?;
        reject_special(&entry.path(), &metadata)?;
        if !metadata.is_file() {
            return Err(ArchiveError::NonRegular {
                path: entry.path().display().to_string(),
            });
        }
        let archive_entry =
            optimized_or_streamed_entry(name, &entry.path(), metadata.len(), "project document")?;
        plan.insert(archive_entry)?;
    }
    Ok(())
}

pub(crate) fn unique_temp_path(target: &Path, purpose: &str) -> Result<PathBuf, ArchiveError> {
    let parent = target.parent().ok_or_else(|| ArchiveError::Publish {
        reason: format!("output '{}' has no parent directory", target.display()),
    })?;
    let file_name = target
        .file_name()
        .and_then(|value| value.to_str())
        .ok_or_else(|| ArchiveError::Publish {
            reason: format!("output '{}' has no portable file name", target.display()),
        })?;
    for _ in 0..128 {
        let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let candidate = parent.join(format!(
            ".{file_name}.mcmpu-{purpose}-{}-{sequence}",
            std::process::id()
        ));
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&candidate)
        {
            Ok(file) => {
                drop(file);
                fs::remove_file(&candidate).map_err(|source| io_error(&candidate, source))?;
                return Ok(candidate);
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(source) => return Err(io_error(&candidate, source)),
        }
    }
    Err(ArchiveError::Publish {
        reason: format!(
            "could not allocate a unique temporary path beside '{}'",
            target.display()
        ),
    })
}

pub(crate) fn publish_pair(
    client_temp: &Path,
    client_target: &Path,
    server_temp: &Path,
    server_target: &Path,
) -> Result<(), ArchiveError> {
    publish_pair_with(
        client_temp,
        client_target,
        server_temp,
        server_target,
        &RealRename,
    )
}

pub(crate) fn publish_single(temp: &Path, target: &Path) -> Result<(), ArchiveError> {
    publish_single_with(temp, target, &RealRename)
}

trait RenameOperation {
    fn rename(&self, from: &Path, to: &Path) -> std::io::Result<()>;
}

struct RealRename;

impl RenameOperation for RealRename {
    fn rename(&self, from: &Path, to: &Path) -> std::io::Result<()> {
        fs::rename(from, to)
    }
}

fn publish_single_with<R: RenameOperation>(
    temp: &Path,
    target: &Path,
    rename: &R,
) -> Result<(), ArchiveError> {
    let backup = unique_backup_path(target)?;
    let had_target = target.exists();

    if had_target {
        rename
            .rename(target, &backup)
            .map_err(|source| ArchiveError::Publish {
                reason: format!("failed to back up '{}': {source}", target.display()),
            })?;
    }

    if let Err(source) = rename.rename(temp, target) {
        if had_target {
            rename
                .rename(&backup, target)
                .map_err(|restore| ArchiveError::Rollback {
                    reason: format!(
                        "failed to restore '{}' after publish error '{source}': {restore}",
                        target.display()
                    ),
                })?;
        }
        return Err(ArchiveError::Publish {
            reason: format!("failed to publish '{}': {source}", target.display()),
        });
    }

    if had_target {
        fs::remove_file(&backup).map_err(|source| io_error(&backup, source))?;
    }
    Ok(())
}

fn publish_pair_with<R: RenameOperation>(
    client_temp: &Path,
    client_target: &Path,
    server_temp: &Path,
    server_target: &Path,
    rename: &R,
) -> Result<(), ArchiveError> {
    let client_backup = unique_backup_path(client_target)?;
    let server_backup = unique_backup_path(server_target)?;
    let paths = PairPublishPaths {
        client_temp,
        client_target,
        client_backup: &client_backup,
        server_temp,
        server_target,
        server_backup: &server_backup,
    };
    let had_client = client_target.exists();
    let had_server = server_target.exists();
    let mut client_backup_moved = false;

    if had_client {
        if let Err(source) = rename.rename(client_target, &client_backup) {
            return Err(cleanup_pair_failure(
                rename,
                paths,
                format!("failed to back up '{}': {source}", client_target.display()),
                PairRollbackState::default(),
            ));
        }
        client_backup_moved = true;
    }
    if had_server && let Err(source) = rename.rename(server_target, &server_backup) {
        return Err(cleanup_pair_failure(
            rename,
            paths,
            format!("failed to back up '{}': {source}", server_target.display()),
            PairRollbackState {
                restore_client: client_backup_moved,
                ..PairRollbackState::default()
            },
        ));
    }
    let server_backup_moved = had_server;

    if let Err(source) = rename.rename(client_temp, client_target) {
        return Err(cleanup_pair_failure(
            rename,
            paths,
            format!("failed to publish '{}': {source}", client_target.display()),
            PairRollbackState {
                restore_client: client_backup_moved,
                restore_server: server_backup_moved,
                remove_client_target: true,
                remove_server_target: false,
            },
        ));
    }
    if let Err(source) = rename.rename(server_temp, server_target) {
        return Err(cleanup_pair_failure(
            rename,
            paths,
            format!("failed to publish '{}': {source}", server_target.display()),
            PairRollbackState {
                restore_client: client_backup_moved,
                restore_server: server_backup_moved,
                remove_client_target: true,
                remove_server_target: true,
            },
        ));
    }

    let mut cleanup_failures = Vec::new();
    if client_backup_moved && let Err(reason) = remove_file_for_publish(&client_backup) {
        cleanup_failures.push(reason);
    }
    if server_backup_moved && let Err(reason) = remove_file_for_publish(&server_backup) {
        cleanup_failures.push(reason);
    }
    if !cleanup_failures.is_empty() {
        return Err(ArchiveError::Publish {
            reason: format!(
                "published client '{}' and server '{}', but archive transaction cleanup failed: {}",
                client_target.display(),
                server_target.display(),
                cleanup_failures.join("; ")
            ),
        });
    }
    Ok(())
}

#[derive(Clone, Copy)]
struct PairPublishPaths<'a> {
    client_temp: &'a Path,
    client_target: &'a Path,
    client_backup: &'a Path,
    server_temp: &'a Path,
    server_target: &'a Path,
    server_backup: &'a Path,
}

#[derive(Clone, Copy, Default)]
struct PairRollbackState {
    restore_client: bool,
    restore_server: bool,
    remove_client_target: bool,
    remove_server_target: bool,
}

fn cleanup_pair_failure<R: RenameOperation>(
    rename: &R,
    paths: PairPublishPaths<'_>,
    original_failure: String,
    state: PairRollbackState,
) -> ArchiveError {
    let mut rollback_failures = Vec::new();
    if state.remove_client_target
        && let Err(reason) = remove_file_for_publish(paths.client_target)
    {
        rollback_failures.push(reason);
    }
    if state.remove_server_target
        && let Err(reason) = remove_file_for_publish(paths.server_target)
    {
        rollback_failures.push(reason);
    }
    if let Err(reason) = restore_backups(rename, paths, state) {
        rollback_failures.push(reason);
    }
    if let Err(reason) = cleanup_temp_files(paths) {
        rollback_failures.push(reason);
    }
    if rollback_failures.is_empty() {
        ArchiveError::Publish {
            reason: original_failure,
        }
    } else {
        ArchiveError::Rollback {
            reason: format!(
                "original failure: {original_failure}; rollback failures: {}",
                rollback_failures.join("; ")
            ),
        }
    }
}

fn restore_backups<R: RenameOperation>(
    rename: &R,
    paths: PairPublishPaths<'_>,
    state: PairRollbackState,
) -> Result<(), String> {
    let mut failures = Vec::new();
    if state.restore_client
        && let Err(source) = rename.rename(paths.client_backup, paths.client_target)
    {
        failures.push(format!(
            "failed to restore client archive '{}' from backup '{}': {source}",
            paths.client_target.display(),
            paths.client_backup.display()
        ));
    }
    if state.restore_server
        && let Err(source) = rename.rename(paths.server_backup, paths.server_target)
    {
        failures.push(format!(
            "failed to restore server archive '{}' from backup '{}': {source}",
            paths.server_target.display(),
            paths.server_backup.display()
        ));
    }
    if failures.is_empty() {
        Ok(())
    } else {
        Err(failures.join("; "))
    }
}

fn cleanup_temp_files(paths: PairPublishPaths<'_>) -> Result<(), String> {
    let mut failures = Vec::new();
    for path in [paths.client_temp, paths.server_temp] {
        if let Err(reason) = remove_file_for_publish(path) {
            failures.push(reason);
        }
    }
    if failures.is_empty() {
        Ok(())
    } else {
        Err(failures.join("; "))
    }
}

fn remove_file_for_publish(path: &Path) -> Result<(), String> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(format!("failed to remove '{}': {source}", path.display())),
    }
}

fn unique_backup_path(target: &Path) -> Result<PathBuf, ArchiveError> {
    unique_temp_path(target, "backup")
}

fn write_entry<W: Write + Seek>(
    zip: &mut zip::ZipWriter<W>,
    entry: &ArchiveEntry,
    output: &Path,
) -> Result<(), ArchiveError> {
    let method = match entry.compression {
        Compression::Deflated => zip::CompressionMethod::Deflated,
        Compression::Stored => zip::CompressionMethod::Stored,
    };
    let mut options = SimpleFileOptions::default().compression_method(method);
    if let Some(mode) = entry.unix_mode {
        options = options.unix_permissions(mode);
    }
    zip.start_file(&entry.name, options)
        .map_err(|source| ArchiveError::Zip {
            path: output.display().to_string(),
            source,
        })?;
    match &entry.source {
        EntrySource::Bytes(bytes) => zip
            .write_all(bytes)
            .map_err(|source| io_error(output, source)),
        EntrySource::File {
            path,
            size,
            expected_sha256,
        } => stream_file(zip, path, *size, expected_sha256.as_deref(), output),
    }
}

fn stream_file<W: Write>(
    output: &mut W,
    source: &Path,
    expected_size: u64,
    expected_sha256: Option<&str>,
    archive_path: &Path,
) -> Result<(), ArchiveError> {
    let (mut file, opened) = open_regular_file_nofollow(source)?;
    if opened.len() != expected_size {
        return Err(ArchiveError::ChangedSource {
            path: source.display().to_string(),
        });
    }
    let mut hasher = expected_sha256.map(|_| Sha256::new());
    let mut copied = 0_u64;
    let mut buffer = vec![0_u8; STREAM_BUFFER_SIZE];
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|error| io_error(source, error))?;
        if read == 0 {
            break;
        }
        output
            .write_all(&buffer[..read])
            .map_err(|error| io_error(archive_path, error))?;
        if let Some(hasher) = &mut hasher {
            hasher.update(&buffer[..read]);
        }
        copied = copied
            .checked_add(read as u64)
            .ok_or(ArchiveError::ChangedSource {
                path: source.display().to_string(),
            })?;
    }
    if copied != expected_size {
        return Err(ArchiveError::ChangedSource {
            path: source.display().to_string(),
        });
    }
    if let (Some(expected), Some(hasher)) = (expected_sha256, hasher) {
        let actual = format!("{:x}", hasher.finalize());
        if actual != expected {
            return Err(ArchiveError::Integrity {
                path: source.display().to_string(),
                expected: expected.to_string(),
                actual,
            });
        }
    }
    Ok(())
}

/// Opens a source exactly once without following a final-component link. The returned metadata is
/// from that same handle, so a replacement after opening cannot redirect archive reads elsewhere.
fn open_regular_file_nofollow(source: &Path) -> Result<(File, fs::Metadata), ArchiveError> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
        options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    }
    let file = options
        .open(source)
        .map_err(|error| io_error(source, error))?;
    let metadata = file.metadata().map_err(|error| io_error(source, error))?;
    reject_special(source, &metadata)?;
    if !metadata.is_file() {
        return Err(ArchiveError::NonRegular {
            path: source.display().to_string(),
        });
    }
    Ok((file, metadata))
}

fn reject_special(path: &Path, metadata: &fs::Metadata) -> Result<(), ArchiveError> {
    if metadata.file_type().is_symlink() {
        return Err(ArchiveError::Link {
            path: path.display().to_string(),
        });
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
        if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return Err(ArchiveError::Link {
                path: path.display().to_string(),
            });
        }
    }
    Ok(())
}

fn reject_reserved_override(name: &str, source: &Path) -> Result<(), ArchiveError> {
    let identity = portable_path_identity(name).map_err(|reason| ArchiveError::UnsafePath {
        path: name.to_string(),
        reason: reason.to_string(),
    })?;
    let logical = identity.strip_prefix("overrides/").unwrap_or(&identity);
    let first = logical.split('/').next().unwrap_or_default();
    let reserved = first == "runtime"
        || first == ".mcsdt"
        || matches!(
            logical,
            "server-install.json"
                | "mcserverdownloadtool-linux-x86_64"
                | "mcserverdownloadtool-macos-aarch64"
                | "mcserverdownloadtool-windows-x86_64.exe"
                | "mc-server-download-tool"
                | "mc-server-download-tool.exe"
                | "server-files.json"
                | "start.bat"
                | "start.sh"
                | "start.bat.new"
                | "start.sh.new"
                | "mcmpu-download.ps1"
                | "mcmpu-check-manual.ps1"
                | "missing-files.txt"
        );
    if reserved {
        return Err(ArchiveError::Reserved {
            source_path: source.display().to_string(),
            path: name.to_string(),
        });
    }
    Ok(())
}

fn optimized_or_streamed_entry(
    name: String,
    source: &Path,
    size: u64,
    origin: &'static str,
) -> Result<ArchiveEntry, ArchiveError> {
    let extension = source
        .extension()
        .and_then(|value| value.to_str())
        .map(str::to_ascii_lowercase);
    if size <= MAX_IN_MEMORY_OPTIMIZATION_SIZE
        && matches!(extension.as_deref(), Some("json" | "png"))
    {
        let (mut file, metadata) = open_regular_file_nofollow(source)?;
        if metadata.len() != size {
            return Err(ArchiveError::ChangedSource {
                path: source.display().to_string(),
            });
        }
        let mut bytes = Vec::with_capacity(metadata.len() as usize);
        file.read_to_end(&mut bytes)
            .map_err(|error| io_error(source, error))?;
        if bytes.len() as u64 != size {
            return Err(ArchiveError::ChangedSource {
                path: source.display().to_string(),
            });
        }
        let optimized = match extension.as_deref() {
            Some("json") => match minify_json_bytes(&bytes) {
                Ok(value) => value,
                Err(error) => {
                    eprintln!(
                        "[build] warning: skipped JSON minify for '{}': {error}",
                        source.display()
                    );
                    bytes
                }
            },
            Some("png") => {
                optimize_png_bytes(&bytes).map_err(|source_error| ArchiveError::OptimizePng {
                    path: source.display().to_string(),
                    source: source_error,
                })?
            }
            _ => unreachable!("extension was checked above"),
        };
        return Ok(ArchiveEntry::bytes(
            name,
            optimized,
            Compression::Deflated,
            origin,
        ));
    }
    Ok(ArchiveEntry::file(
        name,
        source.to_path_buf(),
        size,
        Compression::Deflated,
        origin,
    ))
}

fn slash_path(path: &Path) -> Result<String, ArchiveError> {
    let mut components = Vec::new();
    for component in path.components() {
        let value = component
            .as_os_str()
            .to_str()
            .ok_or_else(|| ArchiveError::UnsafePath {
                path: path.display().to_string(),
                reason: "path is not valid Unicode".to_string(),
            })?;
        components.push(value);
    }
    Ok(components.join("/"))
}

fn io_error(path: &Path, source: std::io::Error) -> ArchiveError {
    ArchiveError::Io {
        path: path.display().to_string(),
        source,
    }
}

#[derive(Debug, Error)]
pub(crate) enum ArchiveError {
    #[error("unsafe archive path '{path}': {reason}")]
    UnsafePath { path: String, reason: String },
    #[error("archive path '{path}' is declared by both {first} and {second}")]
    Conflict {
        path: String,
        first: String,
        second: String,
    },
    #[error("archive entry count exceeds limit {limit}")]
    EntryLimit { limit: usize },
    #[error("archive file '{path}' is {size} bytes, exceeding limit {limit}")]
    FileLimit { path: String, size: u64, limit: u64 },
    #[error("archive uncompressed size {size} bytes exceeds limit {limit}")]
    TotalLimit { size: u64, limit: u64 },
    #[error("symbolic link or reparse point is forbidden: '{path}'")]
    Link { path: String },
    #[error("non-regular archive source is forbidden: '{path}'")]
    NonRegular { path: String },
    #[error("override root is not a directory: '{path}'")]
    NonDirectory { path: String },
    #[error("walked path escaped its override root: '{path}'")]
    EscapedRoot { path: String },
    #[error("override source '{source_path}' targets installer-reserved root path '{path}'")]
    Reserved { source_path: String, path: String },
    #[error("archive source changed while building: '{path}'")]
    ChangedSource { path: String },
    #[error("archive source '{path}' SHA-256 mismatch: expected {expected}, found {actual}")]
    Integrity {
        path: String,
        expected: String,
        actual: String,
    },
    #[error("failed to access '{path}': {source}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to write ZIP '{path}': {source}")]
    Zip {
        path: String,
        #[source]
        source: zip::result::ZipError,
    },
    #[error("failed to optimize PNG '{path}': {source}")]
    OptimizePng {
        path: String,
        #[source]
        source: oxipng::PngError,
    },
    #[error("failed to publish archive pair transactionally: {reason}")]
    Publish { reason: String },
    #[error("failed to roll back archive pair: {reason}")]
    Rollback { reason: String },
    #[error("failed to walk '{path}': {source}")]
    Walk {
        path: String,
        #[source]
        source: walkdir::Error,
    },
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    #[test]
    fn low_limits_reject_entries_files_and_total_size() {
        let limits = ArchiveLimits {
            max_entries: 1,
            max_file_size: 3,
            max_total_uncompressed: 3,
        };
        let mut plan = ArchivePlan::new(limits);
        plan.insert(ArchiveEntry::bytes(
            "a",
            vec![1, 2, 3],
            Compression::Stored,
            "a",
        ))
        .unwrap();
        assert!(matches!(
            plan.insert(ArchiveEntry::bytes("b", vec![1], Compression::Stored, "b")),
            Err(ArchiveError::EntryLimit { .. })
        ));
        let mut plan = ArchivePlan::new(limits);
        assert!(matches!(
            plan.insert(ArchiveEntry::bytes(
                "a",
                vec![0; 4],
                Compression::Stored,
                "a"
            )),
            Err(ArchiveError::FileLimit { .. })
        ));
        let mut plan = ArchivePlan::new(ArchiveLimits {
            max_entries: 2,
            max_file_size: 3,
            max_total_uncompressed: 3,
        });
        plan.insert(ArchiveEntry::bytes(
            "a",
            vec![0; 2],
            Compression::Stored,
            "a",
        ))
        .unwrap();
        assert!(matches!(
            plan.insert(ArchiveEntry::bytes(
                "b",
                vec![0; 2],
                Compression::Stored,
                "b"
            )),
            Err(ArchiveError::TotalLimit { .. })
        ));
    }

    #[test]
    fn second_publish_failure_restores_previous_pair() {
        struct FailSecondPublish {
            calls: AtomicUsize,
        }
        impl RenameOperation for FailSecondPublish {
            fn rename(&self, from: &Path, to: &Path) -> std::io::Result<()> {
                let call = self.calls.fetch_add(1, Ordering::SeqCst);
                if call == 3 {
                    return Err(std::io::Error::other("injected second publish failure"));
                }
                fs::rename(from, to)
            }
        }

        let temp = tempfile::tempdir().unwrap();
        let client = temp.path().join("client.zip");
        let server = temp.path().join("server.zip");
        let client_new = temp.path().join("client.tmp");
        let server_new = temp.path().join("server.tmp");
        fs::write(&client, b"old-client").unwrap();
        fs::write(&server, b"old-server").unwrap();
        fs::write(&client_new, b"new-client").unwrap();
        fs::write(&server_new, b"new-server").unwrap();
        let error = publish_pair_with(
            &client_new,
            &client,
            &server_new,
            &server,
            &FailSecondPublish {
                calls: AtomicUsize::new(0),
            },
        )
        .unwrap_err();
        assert!(matches!(error, ArchiveError::Publish { .. }));
        assert_eq!(fs::read(client).unwrap(), b"old-client");
        assert_eq!(fs::read(server).unwrap(), b"old-server");
        assert!(!client_new.exists());
        assert!(!server_new.exists());
    }

    #[test]
    fn server_backup_failure_reports_client_restore_failure() {
        struct FailServerBackupAndClientRestore {
            calls: AtomicUsize,
        }
        impl RenameOperation for FailServerBackupAndClientRestore {
            fn rename(&self, from: &Path, to: &Path) -> std::io::Result<()> {
                match self.calls.fetch_add(1, Ordering::SeqCst) {
                    1 => Err(std::io::Error::other("injected server backup failure")),
                    2 => Err(std::io::Error::other("injected client restore failure")),
                    _ => fs::rename(from, to),
                }
            }
        }

        let temp = tempfile::tempdir().unwrap();
        let client = temp.path().join("client.zip");
        let server = temp.path().join("server.zip");
        let client_new = temp.path().join("client.tmp");
        let server_new = temp.path().join("server.tmp");
        fs::write(&client, b"old-client").unwrap();
        fs::write(&server, b"old-server").unwrap();
        fs::write(&client_new, b"new-client").unwrap();
        fs::write(&server_new, b"new-server").unwrap();

        let error = publish_pair_with(
            &client_new,
            &client,
            &server_new,
            &server,
            &FailServerBackupAndClientRestore {
                calls: AtomicUsize::new(0),
            },
        )
        .unwrap_err();

        let reason = match error {
            ArchiveError::Rollback { reason } => reason,
            other => panic!("expected rollback error, found {other}"),
        };
        assert!(reason.contains("injected server backup failure"));
        assert!(reason.contains("injected client restore failure"));
        assert!(reason.contains("failed to restore client archive"));
        assert!(!client.exists());
        assert_eq!(fs::read(server).unwrap(), b"old-server");
        assert!(!client_new.exists());
        assert!(!server_new.exists());

        let backup_files = fs::read_dir(temp.path())
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| entry.file_name().to_string_lossy().contains("backup"))
            .collect::<Vec<_>>();
        assert_eq!(backup_files.len(), 1);
        assert_eq!(fs::read(backup_files[0].path()).unwrap(), b"old-client");
    }

    #[test]
    fn streamed_verified_asset_rejects_corruption() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("asset");
        fs::write(&source, b"asset").unwrap();
        let mut output = Vec::new();
        let error = stream_file(
            &mut output,
            &source,
            5,
            Some(&format!("{:x}", Sha256::digest(b"other"))),
            &temp.path().join("archive"),
        )
        .unwrap_err();
        assert!(matches!(error, ArchiveError::Integrity { .. }));
    }

    #[test]
    fn default_file_limit_is_eight_gibibytes() {
        assert_eq!(DEFAULT_LIMITS.max_file_size, 8 * 1024 * 1024 * 1024);
    }

    #[cfg(unix)]
    #[test]
    fn streaming_open_rejects_a_link_replacement_without_reading_its_target() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        let outside = temp.path().join("outside");
        fs::write(&source, b"inside").unwrap();
        fs::write(&outside, b"outside").unwrap();
        fs::remove_file(&source).unwrap();
        symlink(&outside, &source).unwrap();

        let error = stream_file(
            &mut Vec::new(),
            &source,
            6,
            None,
            &temp.path().join("archive"),
        )
        .unwrap_err();
        assert!(matches!(
            error,
            ArchiveError::Io { .. } | ArchiveError::Link { .. }
        ));
    }

    #[cfg(unix)]
    #[test]
    fn traversal_rejects_symlink() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("overrides");
        fs::create_dir(&root).unwrap();
        fs::write(temp.path().join("outside"), b"outside").unwrap();
        symlink(temp.path().join("outside"), root.join("linked")).unwrap();
        let mut plan = ArchivePlan::new(DEFAULT_LIMITS);
        assert!(matches!(
            collect_layer(&mut plan, &root, None, "common", None, true),
            Err(ArchiveError::Link { .. })
        ));
    }

    #[cfg(unix)]
    #[test]
    fn traversal_rejects_non_regular_socket() {
        use std::os::unix::net::UnixListener;

        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("overrides");
        fs::create_dir(&root).unwrap();
        let _listener = UnixListener::bind(root.join("runtime.sock")).unwrap();
        let mut plan = ArchivePlan::new(DEFAULT_LIMITS);
        assert!(matches!(
            collect_layer(&mut plan, &root, None, "common", None, true),
            Err(ArchiveError::NonRegular { .. })
        ));
    }

    #[cfg(windows)]
    #[test]
    fn traversal_rejects_directory_reparse_point() {
        use std::os::windows::fs::MetadataExt;
        use std::process::Command;

        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("overrides");
        let outside = temp.path().join("outside");
        let junction = root.join("junction");
        fs::create_dir(&root).unwrap();
        fs::create_dir(&outside).unwrap();
        fs::write(outside.join("file"), b"outside").unwrap();
        let status = Command::new("cmd.exe")
            .args(["/D", "/C", "mklink", "/J"])
            .arg(&junction)
            .arg(&outside)
            .status()
            .unwrap();
        assert!(
            status.success(),
            "failed to create a test directory junction"
        );
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
        let metadata = fs::symlink_metadata(&junction).unwrap();
        assert_ne!(
            metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT,
            0,
            "junction fixture is not a reparse point"
        );
        let mut plan = ArchivePlan::new(DEFAULT_LIMITS);
        assert!(matches!(
            collect_layer(&mut plan, &root, None, "common", None, true),
            Err(ArchiveError::Link { .. })
        ));
    }

    #[cfg(windows)]
    #[test]
    fn streaming_open_rejects_a_file_link_replacement_without_reading_its_target() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        let outside = temp.path().join("outside");
        fs::write(&source, b"inside").unwrap();
        fs::write(&outside, b"outside").unwrap();
        fs::remove_file(&source).unwrap();
        match std::os::windows::fs::symlink_file(&outside, &source) {
            Ok(()) => {}
            Err(error)
                if error.raw_os_error() == Some(1314)
                    || error.kind() == std::io::ErrorKind::PermissionDenied =>
            {
                return;
            }
            Err(error) => panic!("failed to create a test file symbolic link: {error}"),
        }

        let error = stream_file(
            &mut Vec::new(),
            &source,
            6,
            None,
            &temp.path().join("archive"),
        )
        .unwrap_err();
        assert!(matches!(
            error,
            ArchiveError::Link { .. } | ArchiveError::Io { .. }
        ));
    }

    #[test]
    fn archive_identity_rejects_case_aliases_and_only_allows_explicit_layer_override() {
        let mut plan = ArchivePlan::new(DEFAULT_LIMITS);
        plan.insert_layer(
            ArchiveEntry::bytes(
                "Config/Example.cfg",
                b"common".to_vec(),
                Compression::Deflated,
                "common",
            ),
            None,
        )
        .unwrap();
        assert!(matches!(
            plan.insert_layer(
                ArchiveEntry::bytes(
                    "config/example.CFG",
                    b"duplicate".to_vec(),
                    Compression::Deflated,
                    "common",
                ),
                None,
            ),
            Err(ArchiveError::Conflict { .. })
        ));
        plan.insert_layer(
            ArchiveEntry::bytes(
                "config/example.CFG",
                b"client".to_vec(),
                Compression::Deflated,
                "client",
            ),
            Some("common"),
        )
        .unwrap();
    }
}
