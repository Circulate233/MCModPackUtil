use std::ffi::OsStr;
use std::path::Path;
use std::process::Command;

use mcmpu::build_version::{emit_git_rerun_dependencies, resolve_build_version};

fn git(repository: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(repository)
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {:?}: {}",
        args,
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_string()
}

fn repository() -> tempfile::TempDir {
    let repository = tempfile::tempdir().unwrap();
    git(repository.path(), &["init"]);
    git(
        repository.path(),
        &["config", "user.email", "version-tests@example.invalid"],
    );
    git(repository.path(), &["config", "user.name", "Version Tests"]);
    repository
}

fn commit(repository: &tempfile::TempDir, name: &str, contents: &str) {
    std::fs::write(repository.path().join(name), contents).unwrap();
    git(repository.path(), &["add", name]);
    git(repository.path(), &["commit", "-m", contents]);
}

#[test]
fn no_tags_uses_zero_baseline_and_head_hash() {
    let repository = repository();
    commit(&repository, "one", "one");

    let hash = git(repository.path(), &["rev-parse", "HEAD"]);
    let version = resolve_build_version(repository.path(), None).unwrap();

    assert_eq!(version.value, format!("0.0.0+{}", &hash[..7]));
}

#[test]
fn invalid_tags_are_ignored() {
    let repository = repository();
    commit(&repository, "one", "one");
    for tag in ["v1.2", "v01.2.3", "v1.2.3.4", "release"] {
        git(repository.path(), &["tag", tag]);
    }

    let hash = git(repository.path(), &["rev-parse", "HEAD"]);
    let version = resolve_build_version(repository.path(), None).unwrap();

    assert_eq!(version.value, format!("0.0.0+{}", &hash[..7]));
}

#[test]
fn nearest_reachable_legal_tag_is_used_even_with_a_later_invalid_tag() {
    let repository = repository();
    commit(&repository, "one", "one");
    git(repository.path(), &["tag", "v1.2.3"]);
    commit(&repository, "two", "two");
    git(repository.path(), &["tag", "v2.3.4"]);
    commit(&repository, "three", "three");
    git(repository.path(), &["tag", "v09.9.9"]);

    let hash = git(repository.path(), &["rev-parse", "HEAD"]);
    let version = resolve_build_version(repository.path(), None).unwrap();

    assert_eq!(version.value, format!("2.3.4+{}", &hash[..7]));
}

#[test]
fn head_at_a_legal_tag_still_includes_the_head_hash() {
    let repository = repository();
    commit(&repository, "one", "one");
    git(repository.path(), &["tag", "v1.2.3"]);

    let hash = git(repository.path(), &["rev-parse", "HEAD"]);
    let version = resolve_build_version(repository.path(), None).unwrap();

    assert_eq!(version.value, format!("1.2.3+{}", &hash[..7]));
}

#[test]
fn dirty_worktree_does_not_change_the_resolved_version_format() {
    let repository = repository();
    commit(&repository, "one", "one");
    git(repository.path(), &["tag", "v4.5.6"]);
    std::fs::write(repository.path().join("one"), "dirty").unwrap();

    let hash = git(repository.path(), &["rev-parse", "HEAD"]);
    let version = resolve_build_version(repository.path(), None).unwrap();

    assert_eq!(version.value, format!("4.5.6+{}", &hash[..7]));
}

#[test]
fn explicit_release_override_is_pure_and_does_not_require_git() {
    let repository = tempfile::tempdir().unwrap();

    let version = resolve_build_version(repository.path(), Some(OsStr::new("3.14.15"))).unwrap();

    assert_eq!(version.value, "3.14.15");
}

#[test]
fn invalid_release_override_fails_fast() {
    let repository = tempfile::tempdir().unwrap();

    let error = resolve_build_version(repository.path(), Some(OsStr::new("v3.14.15"))).unwrap_err();

    assert!(error.contains("strict X.Y.Z"));
}

#[test]
fn missing_git_state_without_override_fails_fast() {
    let repository = tempfile::tempdir().unwrap();

    let error = resolve_build_version(repository.path(), None).unwrap_err();

    assert!(error.contains("Git"));
}

#[test]
fn worktree_dependency_declaration_tracks_worktree_and_common_git_state() {
    let repository = repository();
    commit(&repository, "one", "one");
    let worktree = tempfile::tempdir().unwrap();
    git(
        repository.path(),
        &["worktree", "add", worktree.path().to_str().unwrap(), "HEAD"],
    );

    emit_git_rerun_dependencies(worktree.path()).unwrap();
    let version = resolve_build_version(worktree.path(), None).unwrap();
    let hash = git(worktree.path(), &["rev-parse", "HEAD"]);

    assert_eq!(version.value, format!("0.0.0+{}", &hash[..7]));
    assert!(worktree.path().join(".git").is_file());
}
