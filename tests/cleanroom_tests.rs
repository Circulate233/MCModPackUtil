use mcmpu::config::{CleanroomConfig, Config};
use mcmpu::dependency::{DependencySide, DependencyType};
use mcmpu::project::cleanroom::{
    cleanroom_dependency_supplements, cleanroom_dependency_supplements_for,
    validate_cleanroom_config,
};

mod support;

#[test]
fn cleanroom_supplements_include_relauncher_client_and_fugue_common() {
    let config = cleanroom_config(false, 3);

    let entries = cleanroom_dependency_supplements(&config).unwrap();

    let relauncher = entries
        .iter()
        .find(|entry| entry.project_id == Some(1_214_906))
        .expect("relauncher entry");
    assert_eq!(
        relauncher.slug.as_deref(),
        Some("cleanroom-relauncher-unofficial")
    );
    assert_eq!(relauncher.file_id, None);
    assert_eq!(relauncher.side, DependencySide::Client);
    assert_eq!(relauncher.dependency_type, DependencyType::Mod);

    let fugue = entries
        .iter()
        .find(|entry| entry.project_id == Some(1_005_815))
        .expect("fugue entry");
    assert_eq!(fugue.slug.as_deref(), Some("fugue"));
    assert_eq!(fugue.file_id, None);
    assert_eq!(fugue.side, DependencySide::Common);
}

#[test]
fn cleanroom_supplements_skip_scalar_when_disabled() {
    let config = cleanroom_config(false, 3);

    let entries = cleanroom_dependency_supplements(&config).unwrap();

    assert!(entries.iter().all(|entry| {
        entry.project_id != Some(1_235_372) && entry.project_id != Some(1_012_168)
    }));
}

#[test]
fn cleanroom_supplements_skip_fugue_when_user_already_lists_it() {
    let config = cleanroom_config(false, 3);
    let existing = vec![entry(1_005_815, "fugue")];

    let entries = cleanroom_dependency_supplements_for(&config, &existing).unwrap();

    assert!(
        entries
            .iter()
            .all(|entry| entry.project_id != Some(1_005_815))
    );
}

#[test]
fn cleanroom_supplements_skip_scalar_when_user_already_lists_scalar() {
    let config = cleanroom_config(true, 3);
    let existing = vec![entry(1_235_372, "scalar")];

    let entries = cleanroom_dependency_supplements_for(&config, &existing).unwrap();

    assert!(entries.iter().all(|entry| {
        entry.project_id != Some(1_235_372) && entry.project_id != Some(1_012_168)
    }));
}

#[test]
fn cleanroom_supplements_skip_relauncher_when_any_relauncher_variant_exists() {
    for project_id in [1_214_490, 1_336_201, 1_214_906] {
        let config = cleanroom_config(false, 3);
        let existing = vec![entry(project_id, "existing-relauncher")];

        let entries = cleanroom_dependency_supplements_for(&config, &existing).unwrap();

        assert!(entries.iter().all(|entry| {
            !matches!(
                entry.project_id,
                Some(1_214_490) | Some(1_336_201) | Some(1_214_906)
            )
        }));
    }
}

#[test]
fn cleanroom_relauncher_config_selects_original_variant() {
    let mut config = cleanroom_config(false, 3);
    config.cleanroom.as_mut().unwrap().relauncher = mcmpu::config::CleanroomRelauncher::Original;

    let entries = cleanroom_dependency_supplements(&config).unwrap();

    let relauncher = entries
        .iter()
        .find(|entry| entry.project_id == Some(1_214_490))
        .expect("original relauncher entry");
    assert_eq!(relauncher.slug.as_deref(), Some("cleanroom-relauncher"));
    assert_eq!(relauncher.side, DependencySide::Client);
}

#[test]
fn cleanroom_relauncher_config_selects_improved_variant() {
    let mut config = cleanroom_config(false, 3);
    config.cleanroom.as_mut().unwrap().relauncher = mcmpu::config::CleanroomRelauncher::Improved;

    let entries = cleanroom_dependency_supplements(&config).unwrap();

    let relauncher = entries
        .iter()
        .find(|entry| entry.project_id == Some(1_336_201))
        .expect("improved relauncher entry");
    assert_eq!(
        relauncher.slug.as_deref(),
        Some("improved-cleanroom-relauncher")
    );
    assert_eq!(relauncher.side, DependencySide::Client);
}

#[test]
fn cleanroom_scalar_two_uses_exact_project_and_file() {
    let config = cleanroom_config(true, 2);

    let entries = cleanroom_dependency_supplements(&config).unwrap();

    let scalar = entries
        .iter()
        .find(|entry| entry.project_id == Some(1_235_372))
        .expect("scalar 2 entry");
    assert_eq!(scalar.file_id, Some(6_842_491));
    assert_eq!(scalar.side, DependencySide::Common);
}

#[test]
fn cleanroom_scalar_three_uses_latest_project() {
    let config = cleanroom_config(true, 3);

    let entries = cleanroom_dependency_supplements(&config).unwrap();

    let scalar = entries
        .iter()
        .find(|entry| entry.project_id == Some(1_012_168))
        .expect("scalar 3 entry");
    assert_eq!(scalar.file_id, None);
    assert_eq!(scalar.side, DependencySide::Common);
}

#[test]
fn cleanroom_scalar_rejects_unsupported_version() {
    let config = cleanroom_config(true, 4);

    let err = cleanroom_dependency_supplements(&config).unwrap_err();

    assert!(err.to_string().contains("scalar_version"));
    assert!(err.to_string().contains("2 or 3"));
}

#[test]
fn cleanroom_section_is_invalid_for_non_cleanroom_loader() {
    let mut config = support::sample_config();
    config.pack.loader = mcmpu::config::LoaderKind::Forge;
    config.cleanroom = Some(CleanroomConfig {
        use_scalar: true,
        scalar_version: 3,
        relauncher: mcmpu::config::CleanroomRelauncher::Unofficial,
    });

    let err = validate_cleanroom_config(&config).unwrap_err();

    assert!(err.to_string().contains("cleanroom"));
}

fn cleanroom_config(use_scalar: bool, scalar_version: u8) -> Config {
    let mut config = support::sample_config();
    config.cleanroom = Some(CleanroomConfig {
        use_scalar,
        scalar_version,
        relauncher: mcmpu::config::CleanroomRelauncher::Unofficial,
    });
    config
}

fn entry(project_id: u64, slug: &str) -> mcmpu::dependency::DependencyEntry {
    mcmpu::dependency::DependencyEntry {
        project_id: Some(project_id),
        file_id: Some(1),
        slug: Some(slug.to_string()),
        loader: Some("forge".to_string()),
        side: DependencySide::Common,
        dependency_type: DependencyType::Mod,
        name: slug.to_string(),
        authors: Vec::new(),
        required: true,
    }
}
