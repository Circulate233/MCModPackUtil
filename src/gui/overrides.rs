//! Project `overrides` layer model for the GUI Overrides page.
//!
//! The page summarises the files a run picks up from `overrides/` and lets each entry be
//! retargeted to `common`, a per-side copy on both sides, or one side only. Rendering stays in
//! `app.rs`; this module owns the on-disk layer semantics.

use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{ErrorKind, Read};
use std::path::{Path, PathBuf};

use thiserror::Error;

use crate::path_safety::validate_portable_relative_path;

/// Buffer size used when comparing two layer copies, so neither is read into memory.
const CONTENT_COMPARE_CHUNK: usize = 64 * 1024;

/// Project layer directories, in application order: `common` first, side layers last.
pub const OVERRIDE_LAYERS: &[OverrideLayer] = &[
    OverrideLayer::Common,
    OverrideLayer::Client,
    OverrideLayer::Server,
];

/// One `overrides/<layer>` directory.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum OverrideLayer {
    Common,
    Client,
    Server,
}

impl OverrideLayer {
    pub fn dir_name(self) -> &'static str {
        match self {
            Self::Common => "common",
            Self::Client => "client",
            Self::Server => "server",
        }
    }
}

/// How the layers that hold one file make it take effect at run time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OverrideScope {
    /// `overrides/common` only: applies to both sides.
    Common,
    /// `overrides/client` only.
    Client,
    /// `overrides/server` only.
    Server,
    /// `overrides/client` and `overrides/server` hold equal copies, so both sides apply it.
    ClientAndServer,
    /// `overrides/common` plus an equal duplicate in one side layer.
    CommonWithSide(OverrideLayer),
    /// Layer copies disagree. Retargeting is refused until the copies match.
    Conflicting,
}

impl OverrideScope {
    /// The offered choice that best describes the current layout.
    ///
    /// A common copy duplicated in one side layer behaves like the common choice: both sides
    /// read the same bytes. A conflicting layout has no honest single choice.
    pub fn choice(self) -> Option<Self> {
        match self {
            Self::Common | Self::CommonWithSide(_) => Some(Self::Common),
            Self::Client => Some(Self::Client),
            Self::Server => Some(Self::Server),
            Self::ClientAndServer => Some(Self::ClientAndServer),
            Self::Conflicting => None,
        }
    }

    /// The scope a retarget writes for the four offered choices.
    pub fn target_layers(self) -> Option<Vec<OverrideLayer>> {
        match self {
            Self::Common => Some(vec![OverrideLayer::Common]),
            Self::Client => Some(vec![OverrideLayer::Client]),
            Self::Server => Some(vec![OverrideLayer::Server]),
            Self::ClientAndServer => Some(vec![OverrideLayer::Client, OverrideLayer::Server]),
            Self::CommonWithSide(_) | Self::Conflicting => None,
        }
    }
}

/// One project-relative file together with every layer that currently holds it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OverrideEntry {
    pub path: String,
    pub scope: OverrideScope,
    /// Size of the copy held by each layer, ordered by [`OVERRIDE_LAYERS`].
    ///
    /// Only metadata is kept: classifying a path needs the sizes, and two copies of equal size
    /// are compared byte by byte from disk on demand. Storing contents instead would keep the
    /// whole `overrides` tree resident, which is megabytes to gigabytes on a large pack.
    pub layers: BTreeMap<OverrideLayer, u64>,
}

impl OverrideEntry {
    /// The state this entry has after [`apply_scope`] writes `scope`.
    ///
    /// Lets the GUI update one row in place instead of rescanning the whole `overrides` tree.
    /// Returns `None` when the scope is not writable or the layer copies disagree.
    pub fn retargeted(&self, scope: OverrideScope) -> Result<Self, OverridesError> {
        let Some(wanted) = scope.target_layers() else {
            return Err(OverridesError::UnsupportedTarget {
                path: self.path.clone(),
            });
        };
        if self.scope == OverrideScope::Conflicting {
            return Err(OverridesError::ConflictingContent {
                path: self.path.clone(),
            });
        }
        let Some(size) = self.layers.values().next().copied() else {
            return Err(OverridesError::NoContent {
                path: self.path.clone(),
            });
        };
        Ok(Self {
            path: self.path.clone(),
            scope,
            layers: wanted.into_iter().map(|layer| (layer, size)).collect(),
        })
    }

    /// Layer directories that hold this file, in application order.
    pub fn present_layers(&self) -> Vec<OverrideLayer> {
        OVERRIDE_LAYERS
            .iter()
            .copied()
            .filter(|layer| self.layers.contains_key(layer))
            .collect()
    }
}

/// The complete `overrides` tree of one project.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct OverrideCatalog {
    pub entries: Vec<OverrideEntry>,
}

impl OverrideCatalog {
    /// Scans `project_dir/overrides`, skipping layer directories that do not exist.
    ///
    /// Only file metadata is collected; layer copies are read from disk only when one path is
    /// held by more than one layer and the copies have to be compared.
    pub fn scan(project_dir: &Path) -> Result<Self, OverridesError> {
        let mut contents: BTreeMap<String, BTreeMap<OverrideLayer, u64>> = BTreeMap::new();
        for layer in OVERRIDE_LAYERS {
            let root = layer_root(project_dir, *layer);
            if !root.exists() {
                continue;
            }
            for entry in walkdir::WalkDir::new(&root) {
                let entry = entry.map_err(|source| OverridesError::Walk {
                    path: root.display().to_string(),
                    source: Box::new(source),
                })?;
                if !entry.file_type().is_file() {
                    continue;
                }
                let relative = entry
                    .path()
                    .strip_prefix(&root)
                    .expect("walked under layer root");
                let path = slash_path(relative);
                validate_portable_relative_path(&path).map_err(|error| {
                    OverridesError::InvalidPath {
                        path: path.clone(),
                        reason: error.to_string(),
                    }
                })?;
                let size = entry
                    .metadata()
                    .map_err(|source| OverridesError::Walk {
                        path: entry.path().display().to_string(),
                        source: Box::new(source),
                    })?
                    .len();
                contents.entry(path).or_default().insert(*layer, size);
            }
        }
        let entries = contents
            .into_iter()
            .map(|(path, layers)| OverrideEntry {
                scope: scope_for(project_dir, &path, &layers),
                path,
                layers,
            })
            .collect();
        Ok(Self { entries })
    }
}

/// Classifies one path from the layers holding it.
fn scope_for(
    project_dir: &Path,
    path: &str,
    layers: &BTreeMap<OverrideLayer, u64>,
) -> OverrideScope {
    let equal =
        |left: OverrideLayer, right: OverrideLayer| match (layers.get(&left), layers.get(&right)) {
            (Some(left_size), Some(right_size)) if left_size == right_size => {
                layer_copies_equal(project_dir, path, left, right).unwrap_or(false)
            }
            _ => false,
        };
    match (
        layers.contains_key(&OverrideLayer::Common),
        layers.contains_key(&OverrideLayer::Client),
        layers.contains_key(&OverrideLayer::Server),
    ) {
        (true, false, false) => OverrideScope::Common,
        (false, true, false) => OverrideScope::Client,
        (false, false, true) => OverrideScope::Server,
        (false, true, true) if equal(OverrideLayer::Client, OverrideLayer::Server) => {
            OverrideScope::ClientAndServer
        }
        (true, true, false) if equal(OverrideLayer::Common, OverrideLayer::Client) => {
            OverrideScope::CommonWithSide(OverrideLayer::Client)
        }
        (true, false, true) if equal(OverrideLayer::Common, OverrideLayer::Server) => {
            OverrideScope::CommonWithSide(OverrideLayer::Server)
        }
        _ => OverrideScope::Conflicting,
    }
}

/// Compares two layer copies chunk by chunk, so neither whole file is held in memory.
fn layer_copies_equal(
    project_dir: &Path,
    path: &str,
    left: OverrideLayer,
    right: OverrideLayer,
) -> Result<bool, OverridesError> {
    let left_path = layer_file(project_dir, left, path);
    let right_path = layer_file(project_dir, right, path);
    let mut left_file = open_layer_file(&left_path)?;
    let mut right_file = open_layer_file(&right_path)?;
    let mut left_buffer = vec![0u8; CONTENT_COMPARE_CHUNK];
    let mut right_buffer = vec![0u8; CONTENT_COMPARE_CHUNK];
    loop {
        let left_filled = read_chunk(&mut left_file, &mut left_buffer, &left_path)?;
        let right_filled = read_chunk(&mut right_file, &mut right_buffer, &right_path)?;
        if left_filled != right_filled {
            return Ok(false);
        }
        if left_filled == 0 {
            return Ok(true);
        }
        if left_buffer[..left_filled] != right_buffer[..right_filled] {
            return Ok(false);
        }
    }
}

fn open_layer_file(path: &Path) -> Result<File, OverridesError> {
    File::open(path).map_err(|source| OverridesError::ReadFile {
        path: path.display().to_string(),
        source,
    })
}

/// Fills `buffer` unless the file ends first; a short read is retried instead of ending the file.
fn read_chunk(file: &mut File, buffer: &mut [u8], path: &Path) -> Result<usize, OverridesError> {
    let mut filled = 0;
    while filled < buffer.len() {
        match file.read(&mut buffer[filled..]) {
            Ok(0) => break,
            Ok(read) => filled += read,
            Err(source) if source.kind() == ErrorKind::Interrupted => {}
            Err(source) => {
                return Err(OverridesError::ReadFile {
                    path: path.display().to_string(),
                    source,
                });
            }
        }
    }
    Ok(filled)
}

/// Writes the file into exactly the chosen layers and removes it from the others.
///
/// Refuses to run while the existing copies disagree, so one side's edits can never be
/// silently replaced by the other side's content. Content moves file to file, so a large jar or
/// resource pack is never held in memory.
pub fn apply_scope(
    project_dir: &Path,
    entry: &OverrideEntry,
    target: OverrideScope,
) -> Result<(), OverridesError> {
    validate_portable_relative_path(&entry.path).map_err(|error| OverridesError::InvalidPath {
        path: entry.path.clone(),
        reason: error.to_string(),
    })?;
    let Some(wanted) = target.target_layers() else {
        return Err(OverridesError::UnsupportedTarget {
            path: entry.path.clone(),
        });
    };
    if entry.scope == OverrideScope::Conflicting {
        return Err(OverridesError::ConflictingContent {
            path: entry.path.clone(),
        });
    }
    let source = OVERRIDE_LAYERS
        .iter()
        .copied()
        .map(|layer| layer_file(project_dir, layer, &entry.path))
        .find(|file| file.is_file())
        .ok_or_else(|| OverridesError::NoContent {
            path: entry.path.clone(),
        })?;
    // Copy first, drop later: the copy being spread must survive its own layer being removed.
    for layer in &wanted {
        let destination = layer_file(project_dir, *layer, &entry.path);
        if destination == source {
            continue;
        }
        if let Some(parent) = destination.parent() {
            create_dir(parent)?;
        }
        fs::copy(&source, &destination).map_err(|source| OverridesError::WriteFile {
            path: destination.display().to_string(),
            source,
        })?;
    }
    for layer in OVERRIDE_LAYERS {
        if wanted.contains(layer) {
            continue;
        }
        let path = layer_file(project_dir, *layer, &entry.path);
        if path.is_file() {
            fs::remove_file(&path).map_err(|source| OverridesError::RemoveFile {
                path: path.display().to_string(),
                source,
            })?;
        }
    }
    Ok(())
}

/// Deletes one entry from every layer that holds it.
///
/// Layer directories left empty by the deletion are pruned so the tree does not keep empty
/// folders behind. A missing file is not an error: the on-disk state is already what the caller
/// asked for.
pub fn delete_entry(
    project_dir: &Path,
    entry: &OverrideEntry,
) -> Result<Vec<OverrideLayer>, OverridesError> {
    validate_portable_relative_path(&entry.path).map_err(|error| OverridesError::InvalidPath {
        path: entry.path.clone(),
        reason: error.to_string(),
    })?;
    let relative = relative_path(&entry.path);
    let mut removed = Vec::new();
    for layer in OVERRIDE_LAYERS {
        let root = layer_root(project_dir, *layer);
        let path = layer_file(project_dir, *layer, &entry.path);
        match fs::remove_file(&path) {
            Ok(()) => removed.push(*layer),
            Err(source) if source.kind() == ErrorKind::NotFound => {}
            Err(source) => {
                return Err(OverridesError::RemoveFile {
                    path: path.display().to_string(),
                    source,
                });
            }
        }
        prune_empty_directories(&root, &relative);
    }
    Ok(removed)
}

/// Removes now-empty directories above one deleted file, keeping the layer root itself.
fn prune_empty_directories(root: &Path, relative: &Path) {
    let Some(parent) = relative.parent() else {
        return;
    };
    let mut current = Some(parent.to_path_buf());
    while let Some(directory) = current {
        if directory.as_os_str().is_empty() {
            break;
        }
        let absolute = root.join(&directory);
        let empty = fs::read_dir(&absolute)
            .map(|mut entries| entries.next().is_none())
            .unwrap_or(false);
        if !empty || fs::remove_dir(&absolute).is_err() {
            break;
        }
        current = directory.parent().map(Path::to_path_buf);
    }
}

fn layer_root(project_dir: &Path, layer: OverrideLayer) -> PathBuf {
    project_dir.join("overrides").join(layer.dir_name())
}

/// Absolute path of one layer's copy of a project-relative file.
fn layer_file(project_dir: &Path, layer: OverrideLayer, path: &str) -> PathBuf {
    layer_root(project_dir, layer).join(relative_path(path))
}

fn relative_path(path: &str) -> PathBuf {
    path.split('/').collect()
}

fn slash_path(path: &Path) -> String {
    path.components()
        .map(|component| component.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

fn create_dir(path: &Path) -> Result<(), OverridesError> {
    fs::create_dir_all(path).map_err(|source| OverridesError::CreateDir {
        path: path.display().to_string(),
        source,
    })
}

#[derive(Debug, Error)]
pub enum OverridesError {
    #[error("failed to walk overrides directory '{path}': {source}")]
    Walk {
        path: String,
        #[source]
        source: Box<walkdir::Error>,
    },
    #[error("overrides path '{path}' is not portable: {reason}")]
    InvalidPath { path: String, reason: String },
    #[error("failed to read overrides file '{path}': {source}")]
    ReadFile {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to write overrides file '{path}': {source}")]
    WriteFile {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to remove overrides file '{path}': {source}")]
    RemoveFile {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to create overrides directory '{path}': {source}")]
    CreateDir {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("overrides entry '{path}' has no stored content")]
    NoContent { path: String },
    #[error("overrides entry '{path}' cannot be retargeted from its current layout")]
    UnsupportedTarget { path: String },
    #[error("overrides entry '{path}' has different content per layer; align the copies first")]
    ConflictingContent { path: String },
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_file(project: &Path, layer: &str, path: &str, bytes: &[u8]) {
        let target = project.join("overrides").join(layer).join(path);
        fs::create_dir_all(target.parent().unwrap()).unwrap();
        fs::write(target, bytes).unwrap();
    }

    fn entry(project: &Path, path: &str) -> OverrideEntry {
        let catalog = OverrideCatalog::scan(project).unwrap();
        catalog
            .entries
            .into_iter()
            .find(|entry| entry.path == path)
            .expect("entry present")
    }

    #[test]
    fn delete_removes_every_layer_copy_and_prunes_empty_directories() {
        let temp = tempfile::tempdir().unwrap();
        let project = temp.path();
        write_file(project, "client", "config/only-client.cfg", b"client");
        write_file(project, "server", "config/only-server.cfg", b"server");
        write_file(project, "common", "config/shared.cfg", b"shared");
        write_file(project, "common", "config/kept.cfg", b"kept");

        let removed = delete_entry(project, &entry(project, "config/shared.cfg")).unwrap();
        assert_eq!(removed, vec![OverrideLayer::Common]);
        assert!(!project.join("overrides/common/config/shared.cfg").exists());
        // A non-empty directory survives so unrelated entries keep their parent.
        assert!(project.join("overrides/common/config/kept.cfg").is_file());

        for path in ["config/only-client.cfg", "config/only-server.cfg"] {
            delete_entry(project, &entry(project, path)).unwrap();
        }
        assert!(!project.join("overrides/client/config").exists());
        assert!(!project.join("overrides/server/config").exists());
        assert!(project.join("overrides/common/config").is_dir());

        // Each layer root itself is never removed.
        assert!(project.join("overrides/client").is_dir());
        assert!(project.join("overrides/server").is_dir());
    }

    #[test]
    fn delete_removes_split_copies_from_both_sides_and_tolerates_missing_files() {
        let temp = tempfile::tempdir().unwrap();
        let project = temp.path();
        write_file(project, "client", "config/split.cfg", b"same");
        write_file(project, "server", "config/split.cfg", b"same");

        let entry = entry(project, "config/split.cfg");
        assert_eq!(entry.scope, OverrideScope::ClientAndServer);
        let removed = delete_entry(project, &entry).unwrap();
        assert_eq!(removed, vec![OverrideLayer::Client, OverrideLayer::Server]);

        // Deleting again reports nothing missing instead of failing.
        assert!(delete_entry(project, &entry).unwrap().is_empty());
    }

    #[test]
    fn delete_rejects_a_path_that_escapes_the_layer() {
        let temp = tempfile::tempdir().unwrap();
        let project = temp.path();
        write_file(project, "common", "config/kept.cfg", b"kept");
        let entry = OverrideEntry {
            path: "../outside.cfg".to_string(),
            scope: OverrideScope::Common,
            layers: BTreeMap::from([(OverrideLayer::Common, 6)]),
        };
        assert!(matches!(
            delete_entry(project, &entry),
            Err(OverridesError::InvalidPath { .. })
        ));
    }

    #[test]
    fn scan_compares_layer_copies_by_content_not_only_size() {
        let temp = tempfile::tempdir().unwrap();
        let project = temp.path();
        write_file(project, "common", "config/equal.cfg", b"aaaa");
        write_file(project, "client", "config/equal.cfg", b"aaaa");
        write_file(project, "common", "config/differ.cfg", b"bbbb");
        write_file(project, "client", "config/differ.cfg", b"cccc");
        write_file(project, "client", "config/sized.cfg", b"short");
        write_file(project, "server", "config/sized.cfg", b"much longer");

        assert_eq!(
            entry(project, "config/equal.cfg").scope,
            OverrideScope::CommonWithSide(OverrideLayer::Client)
        );
        // Equal sizes with different bytes must not be merged into one scope.
        assert_eq!(
            entry(project, "config/differ.cfg").scope,
            OverrideScope::Conflicting
        );
        assert_eq!(
            entry(project, "config/sized.cfg").scope,
            OverrideScope::Conflicting
        );
    }

    #[test]
    fn apply_scope_copies_bytes_between_layers_and_drops_the_others() {
        let temp = tempfile::tempdir().unwrap();
        let project = temp.path();
        write_file(project, "common", "config/shared.cfg", b"shared");
        write_file(project, "client", "config/shared.cfg", b"shared");

        let entry = entry(project, "config/shared.cfg");
        let target = OverrideScope::ClientAndServer;
        apply_scope(project, &entry, target).unwrap();

        assert!(!project.join("overrides/common/config/shared.cfg").exists());
        assert_eq!(
            fs::read(project.join("overrides/client/config/shared.cfg")).unwrap(),
            b"shared"
        );
        assert_eq!(
            fs::read(project.join("overrides/server/config/shared.cfg")).unwrap(),
            b"shared"
        );
        assert_eq!(entry.retargeted(target).unwrap().scope, target);

        // Retargeting back to a single common copy removes both side copies.
        let updated = entry.retargeted(target).unwrap();
        apply_scope(project, &updated, OverrideScope::Common).unwrap();
        assert!(!project.join("overrides/client/config/shared.cfg").exists());
        assert!(!project.join("overrides/server/config/shared.cfg").exists());
        assert_eq!(
            fs::read(project.join("overrides/common/config/shared.cfg")).unwrap(),
            b"shared"
        );
    }

    #[test]
    fn conflicting_copies_cannot_be_retargeted() {
        let temp = tempfile::tempdir().unwrap();
        let project = temp.path();
        write_file(project, "common", "config/differ.cfg", b"bbbb");
        write_file(project, "client", "config/differ.cfg", b"cccc");

        let entry = entry(project, "config/differ.cfg");
        assert!(matches!(
            entry.retargeted(OverrideScope::Common),
            Err(OverridesError::ConflictingContent { .. })
        ));
        assert!(matches!(
            apply_scope(project, &entry, OverrideScope::Common),
            Err(OverridesError::ConflictingContent { .. })
        ));
    }
}
