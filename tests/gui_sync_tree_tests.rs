#![cfg(feature = "gui")]

use std::path::PathBuf;

use mcmpu::gui::sync_tree::{
    SyncTree, SyncTreeAction, SyncTreeError, SyncTreeNode, apply_tree_actions,
    folder_non_deleted_selection,
};
use mcmpu::project::run::{
    PendingSyncChange, PendingSyncEntry, PendingSyncManifest, RunSide, RunSourceKind, SyncTarget,
};

#[test]
fn sync_tree_groups_paths_and_uses_folder_first_natural_order() {
    let manifest = manifest(vec![
        entry("config/file10.cfg", PendingSyncChange::Modified),
        entry("root10.txt", PendingSyncChange::Added),
        entry("config/file2.cfg", PendingSyncChange::Modified),
        entry("scripts/client/init.cfg", PendingSyncChange::Added),
        entry("root2.txt", PendingSyncChange::Added),
    ]);

    let tree = SyncTree::from_manifest(&manifest).unwrap();

    assert_eq!(
        node_names(&tree.nodes),
        ["config", "scripts", "root2.txt", "root10.txt"]
    );
    let config_children = tree.nodes[0].children().unwrap();
    assert_eq!(node_names(config_children), ["file2.cfg", "file10.cfg"]);
    let scripts_children = tree.nodes[1].children().unwrap();
    assert_eq!(node_names(scripts_children), ["client"]);
    assert_eq!(
        node_names(scripts_children[0].children().unwrap()),
        ["init.cfg"]
    );
}

#[test]
fn folder_batch_selection_never_implicitly_selects_deletions() {
    let mut modified = entry("config/changed.cfg", PendingSyncChange::Modified);
    modified.apply = false;
    let mut deleted = entry("config/deleted.cfg", PendingSyncChange::Deleted);
    deleted.apply = false;
    let mut added = entry("config/new.cfg", PendingSyncChange::Added);
    added.apply = false;
    let mut manifest = manifest(vec![modified, deleted, added]);
    let tree = SyncTree::from_manifest(&manifest).unwrap();
    let indices = tree.nodes[0].descendant_entry_indices();

    apply_tree_actions(
        &mut manifest,
        &[SyncTreeAction::SetFolderNonDeletedApply {
            entry_indices: indices.clone(),
            apply: true,
        }],
    )
    .unwrap();

    assert!(manifest.entries[0].apply);
    assert!(!manifest.entries[1].apply);
    assert!(manifest.entries[2].apply);
    assert_eq!(
        folder_non_deleted_selection(&manifest, &indices),
        Ok((2, 2))
    );

    apply_tree_actions(
        &mut manifest,
        &[
            SyncTreeAction::SetEntryApply {
                entry_index: 1,
                apply: true,
            },
            SyncTreeAction::IgnoreFolder {
                entry_indices: indices,
            },
        ],
    )
    .unwrap();
    assert!(manifest.entries.iter().all(|entry| !entry.apply));
}

#[test]
fn sync_tree_rejects_unsafe_and_duplicate_paths_before_rendering() {
    let unsafe_manifest = manifest(vec![entry("../outside.cfg", PendingSyncChange::Added)]);
    assert!(matches!(
        SyncTree::from_manifest(&unsafe_manifest),
        Err(SyncTreeError::InvalidPath { .. })
    ));

    let duplicate_manifest = manifest(vec![
        entry("config/a.cfg", PendingSyncChange::Added),
        entry("config/a.cfg", PendingSyncChange::Modified),
    ]);
    assert!(matches!(
        SyncTree::from_manifest(&duplicate_manifest),
        Err(SyncTreeError::DuplicatePath { .. })
    ));
}

fn manifest(entries: Vec<PendingSyncEntry>) -> PendingSyncManifest {
    PendingSyncManifest {
        side: RunSide::Client,
        run_dir: PathBuf::from("run/client"),
        path: PathBuf::from(".mcmpu/pending-sync-client.json"),
        ignored_debug: 0,
        ignored_generated: 0,
        entries,
    }
}

fn entry(path: &str, change: PendingSyncChange) -> PendingSyncEntry {
    PendingSyncEntry {
        path: path.to_string(),
        change,
        source: (change != PendingSyncChange::Added).then_some(RunSourceKind::OverridesClient),
        target: SyncTarget::Client,
        apply: false,
    }
}

fn node_names(nodes: &[SyncTreeNode]) -> Vec<&str> {
    nodes.iter().map(SyncTreeNode::name).collect()
}

#[test]
fn filtered_tree_keeps_only_matches_but_preserves_manifest_indices() {
    let manifest = manifest(vec![
        entry("config/keep.cfg", PendingSyncChange::Modified),
        entry("config/other.cfg", PendingSyncChange::Modified),
        entry("scripts/keep.cfg", PendingSyncChange::Added),
    ]);

    let tree =
        SyncTree::from_manifest_filtered(&manifest, |entry| entry.path.contains("keep")).unwrap();

    assert_eq!(node_names(&tree.nodes), ["config", "scripts"]);
    let config_children = tree.nodes[0].children().unwrap();
    assert_eq!(node_names(config_children), ["keep.cfg"]);

    // The visible file still addresses the entry at its original manifest position.
    let index = config_children[0].entry_index().unwrap();
    assert_eq!(index, 0);
    assert_eq!(manifest.entries[index].path, "config/keep.cfg");

    let scripts_index = tree.nodes[1].children().unwrap()[0].entry_index().unwrap();
    assert_eq!(manifest.entries[scripts_index].path, "scripts/keep.cfg");
}

#[test]
fn filtered_tree_is_empty_when_nothing_matches() {
    let manifest = manifest(vec![entry("config/a.cfg", PendingSyncChange::Modified)]);
    let tree = SyncTree::from_manifest_filtered(&manifest, |entry| entry.path.contains("missing"))
        .unwrap();
    assert!(tree.nodes.is_empty());
}

#[test]
fn bulk_select_never_enables_deletions() {
    let manifest = manifest(vec![
        entry("config/changed.cfg", PendingSyncChange::Modified),
        entry("config/removed.cfg", PendingSyncChange::Deleted),
        entry("config/added.cfg", PendingSyncChange::Added),
    ]);
    let mut manifest = manifest;
    let all_indices = (0..manifest.entries.len()).collect::<Vec<_>>();
    apply_tree_actions(
        &mut manifest,
        &[SyncTreeAction::SetFolderNonDeletedApply {
            entry_indices: all_indices,
            apply: true,
        }],
    )
    .unwrap();

    assert!(manifest.entries[0].apply);
    assert!(
        !manifest.entries[1].apply,
        "a deletion needs explicit confirmation"
    );
    assert!(manifest.entries[2].apply);
}
