use mcmpu::project::build::BuildProgress;
use mcmpu::project::check::{check_project_with_api, check_project_with_api_and_progress};

mod support;

#[test]
fn check_validates_project_dependency_resolution_without_writing_zips() {
    let temp = tempfile::tempdir().unwrap();
    support::write_build_project(temp.path());

    let report = check_project_with_api(temp.path(), &support::FixtureApi).unwrap();

    assert_eq!(report.client_manifest_entries, 6);
    assert_eq!(report.server_downloads, 4);
    assert!(!temp.path().join("dist").exists());
}

#[test]
fn check_reports_progress_while_resolving_dependencies() {
    let temp = tempfile::tempdir().unwrap();
    support::write_build_project(temp.path());
    let mut events = Vec::new();

    check_project_with_api_and_progress(temp.path(), &support::FixtureApi, &mut |event| {
        events.push(event.clone());
    })
    .unwrap();

    assert!(matches!(
        events.first(),
        Some(BuildProgress::ReadingProject { .. })
    ));
    assert!(events.iter().any(|event| matches!(
        event,
        BuildProgress::ResolvingDependencies { total } if total > &0
    )));
    assert!(events.iter().any(|event| matches!(
        event,
        BuildProgress::ResolvedDependency { completed, total, .. } if completed == total
    )));
    assert!(!events.iter().any(|event| matches!(
        event,
        BuildProgress::WritingClientZip { .. } | BuildProgress::WritingServerZip { .. }
    )));
}
