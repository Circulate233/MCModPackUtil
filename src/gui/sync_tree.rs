use std::cmp::Ordering;
use std::collections::BTreeSet;

use thiserror::Error;

use crate::project::run::{PendingSyncChange, PendingSyncManifest, SyncTarget};

/// Immutable directory hierarchy derived from one pending-sync manifest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncTree {
    pub nodes: Vec<SyncTreeNode>,
}

/// One folder or file in the pending-sync hierarchy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SyncTreeNode {
    Folder {
        name: String,
        path: String,
        children: Vec<SyncTreeNode>,
    },
    File {
        name: String,
        path: String,
        entry_index: usize,
    },
}

/// Deferred edit produced by the tree UI and applied after rendering completes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SyncTreeAction {
    SetEntryApply {
        entry_index: usize,
        apply: bool,
    },
    SetEntryTarget {
        entry_index: usize,
        target: SyncTarget,
    },
    SetFolderNonDeletedApply {
        entry_indices: Vec<usize>,
        apply: bool,
    },
    IgnoreFolder {
        entry_indices: Vec<usize>,
    },
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum SyncTreeError {
    #[error("pending sync path '{path}' is not a portable relative file path")]
    InvalidPath { path: String },
    #[error("pending sync path '{path}' occurs more than once")]
    DuplicatePath { path: String },
    #[error("pending sync tree references missing entry index {index}")]
    MissingEntry { index: usize },
}

impl SyncTree {
    pub fn from_manifest(manifest: &PendingSyncManifest) -> Result<Self, SyncTreeError> {
        Self::from_manifest_filtered(manifest, |_| true)
    }

    /// Builds the tree from the entries selected by `keep`.
    ///
    /// Entry indices stay relative to the original manifest, so folder actions still address the
    /// same entries the full manifest holds while the window shows a filtered subset.
    pub fn from_manifest_filtered<F>(
        manifest: &PendingSyncManifest,
        keep: F,
    ) -> Result<Self, SyncTreeError>
    where
        F: Fn(&crate::project::run::PendingSyncEntry) -> bool,
    {
        let mut nodes = Vec::new();
        let mut paths = BTreeSet::new();
        for (entry_index, entry) in manifest.entries.iter().enumerate() {
            if !keep(entry) {
                continue;
            }
            let segments = path_segments(&entry.path)?;
            if !paths.insert(entry.path.clone()) {
                return Err(SyncTreeError::DuplicatePath {
                    path: entry.path.clone(),
                });
            }
            insert_entry(&mut nodes, &segments, entry_index, String::new());
        }
        sort_nodes(&mut nodes);
        Ok(Self { nodes })
    }
}

impl SyncTreeNode {
    pub fn name(&self) -> &str {
        match self {
            Self::Folder { name, .. } | Self::File { name, .. } => name,
        }
    }

    pub fn path(&self) -> &str {
        match self {
            Self::Folder { path, .. } | Self::File { path, .. } => path,
        }
    }

    pub fn children(&self) -> Option<&[SyncTreeNode]> {
        match self {
            Self::Folder { children, .. } => Some(children),
            Self::File { .. } => None,
        }
    }

    pub fn entry_index(&self) -> Option<usize> {
        match self {
            Self::File { entry_index, .. } => Some(*entry_index),
            Self::Folder { .. } => None,
        }
    }

    pub fn descendant_entry_indices(&self) -> Vec<usize> {
        let mut indices = Vec::new();
        collect_entry_indices(self, &mut indices);
        indices
    }
}

pub fn folder_non_deleted_selection(
    manifest: &PendingSyncManifest,
    entry_indices: &[usize],
) -> Result<(usize, usize), SyncTreeError> {
    let mut eligible = 0;
    let mut selected = 0;
    for index in entry_indices {
        let entry = manifest
            .entries
            .get(*index)
            .ok_or(SyncTreeError::MissingEntry { index: *index })?;
        if entry.change != PendingSyncChange::Deleted {
            eligible += 1;
            selected += usize::from(entry.apply);
        }
    }
    Ok((selected, eligible))
}

pub fn apply_tree_actions(
    manifest: &mut PendingSyncManifest,
    actions: &[SyncTreeAction],
) -> Result<(), SyncTreeError> {
    for action in actions {
        for index in action_entry_indices(action) {
            if manifest.entries.get(index).is_none() {
                return Err(SyncTreeError::MissingEntry { index });
            }
        }
    }

    for action in actions {
        match action {
            SyncTreeAction::SetEntryApply { entry_index, apply } => {
                manifest.entries[*entry_index].apply = *apply;
            }
            SyncTreeAction::SetEntryTarget {
                entry_index,
                target,
            } => {
                manifest.entries[*entry_index].target = *target;
            }
            SyncTreeAction::SetFolderNonDeletedApply {
                entry_indices,
                apply,
            } => {
                for index in entry_indices {
                    let entry = &mut manifest.entries[*index];
                    if entry.change != PendingSyncChange::Deleted {
                        entry.apply = *apply;
                    }
                }
            }
            SyncTreeAction::IgnoreFolder { entry_indices } => {
                for index in entry_indices {
                    manifest.entries[*index].apply = false;
                }
            }
        }
    }
    Ok(())
}

pub fn natural_name_cmp(left: &str, right: &str) -> Ordering {
    let left = left.as_bytes();
    let right = right.as_bytes();
    let mut left_index = 0;
    let mut right_index = 0;

    while left_index < left.len() && right_index < right.len() {
        if left[left_index].is_ascii_digit() && right[right_index].is_ascii_digit() {
            let left_end = digit_run_end(left, left_index);
            let right_end = digit_run_end(right, right_index);
            let left_significant = trim_numeric_zeroes(&left[left_index..left_end]);
            let right_significant = trim_numeric_zeroes(&right[right_index..right_end]);
            let order = left_significant
                .len()
                .cmp(&right_significant.len())
                .then_with(|| left_significant.cmp(right_significant))
                .then_with(|| (left_end - left_index).cmp(&(right_end - right_index)));
            if order != Ordering::Equal {
                return order;
            }
            left_index = left_end;
            right_index = right_end;
            continue;
        }

        let order = left[left_index]
            .to_ascii_lowercase()
            .cmp(&right[right_index].to_ascii_lowercase())
            .then_with(|| left[left_index].cmp(&right[right_index]));
        if order != Ordering::Equal {
            return order;
        }
        left_index += 1;
        right_index += 1;
    }
    left.len().cmp(&right.len())
}

fn path_segments(path: &str) -> Result<Vec<&str>, SyncTreeError> {
    let invalid =
        path.is_empty() || path.starts_with(['/', '\\']) || path.as_bytes().get(1) == Some(&b':');
    let segments = path.split(['/', '\\']).collect::<Vec<_>>();
    if invalid
        || segments
            .iter()
            .any(|segment| segment.is_empty() || matches!(*segment, "." | ".."))
    {
        return Err(SyncTreeError::InvalidPath {
            path: path.to_string(),
        });
    }
    Ok(segments)
}

fn insert_entry(
    nodes: &mut Vec<SyncTreeNode>,
    segments: &[&str],
    entry_index: usize,
    parent_path: String,
) {
    let name = segments[0];
    let path = if parent_path.is_empty() {
        name.to_string()
    } else {
        format!("{parent_path}/{name}")
    };
    if segments.len() == 1 {
        nodes.push(SyncTreeNode::File {
            name: name.to_string(),
            path,
            entry_index,
        });
        return;
    }

    let folder_index = nodes.iter().position(
        |node| matches!(node, SyncTreeNode::Folder { name: existing, .. } if existing == name),
    );
    let folder_index = folder_index.unwrap_or_else(|| {
        nodes.push(SyncTreeNode::Folder {
            name: name.to_string(),
            path: path.clone(),
            children: Vec::new(),
        });
        nodes.len() - 1
    });
    let SyncTreeNode::Folder { children, .. } = &mut nodes[folder_index] else {
        unreachable!("folder index points to a folder")
    };
    insert_entry(children, &segments[1..], entry_index, path);
}

fn sort_nodes(nodes: &mut [SyncTreeNode]) {
    for node in nodes.iter_mut() {
        if let SyncTreeNode::Folder { children, .. } = node {
            sort_nodes(children);
        }
    }
    nodes.sort_by(|left, right| {
        node_kind_order(left)
            .cmp(&node_kind_order(right))
            .then_with(|| natural_name_cmp(left.name(), right.name()))
            .then_with(|| left.path().cmp(right.path()))
    });
}

fn node_kind_order(node: &SyncTreeNode) -> u8 {
    match node {
        SyncTreeNode::Folder { .. } => 0,
        SyncTreeNode::File { .. } => 1,
    }
}

fn collect_entry_indices(node: &SyncTreeNode, indices: &mut Vec<usize>) {
    match node {
        SyncTreeNode::Folder { children, .. } => {
            for child in children {
                collect_entry_indices(child, indices);
            }
        }
        SyncTreeNode::File { entry_index, .. } => indices.push(*entry_index),
    }
}

fn action_entry_indices(action: &SyncTreeAction) -> Vec<usize> {
    match action {
        SyncTreeAction::SetEntryApply { entry_index, .. }
        | SyncTreeAction::SetEntryTarget { entry_index, .. } => vec![*entry_index],
        SyncTreeAction::SetFolderNonDeletedApply { entry_indices, .. }
        | SyncTreeAction::IgnoreFolder { entry_indices } => entry_indices.clone(),
    }
}

fn digit_run_end(bytes: &[u8], start: usize) -> usize {
    let mut end = start;
    while end < bytes.len() && bytes[end].is_ascii_digit() {
        end += 1;
    }
    end
}

fn trim_numeric_zeroes(bytes: &[u8]) -> &[u8] {
    let first_non_zero = bytes
        .iter()
        .position(|byte| *byte != b'0')
        .unwrap_or(bytes.len().saturating_sub(1));
    &bytes[first_non_zero..]
}
