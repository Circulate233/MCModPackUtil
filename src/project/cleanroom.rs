use thiserror::Error;

use crate::config::{CleanroomRelauncher, Config, LoaderKind};
use crate::dependency::{DependencyEntry, DependencySide, DependencyType};

const CLEANROOM_RELAUNCHER_ORIGINAL_PROJECT_ID: u64 = 1_214_490;
const CLEANROOM_RELAUNCHER_ORIGINAL_SLUG: &str = "cleanroom-relauncher";
const CLEANROOM_RELAUNCHER_UNOFFICIAL_PROJECT_ID: u64 = 1_214_906;
const CLEANROOM_RELAUNCHER_UNOFFICIAL_SLUG: &str = "cleanroom-relauncher-unofficial";
const CLEANROOM_RELAUNCHER_IMPROVED_PROJECT_ID: u64 = 1_336_201;
const CLEANROOM_RELAUNCHER_IMPROVED_SLUG: &str = "improved-cleanroom-relauncher";
const FUGUE_PROJECT_ID: u64 = 1_005_815;
const FUGUE_SLUG: &str = "fugue";
const SCALAR_TWO_PROJECT_ID: u64 = 1_235_372;
const SCALAR_TWO_FILE_ID: u64 = 6_842_491;
const SCALAR_THREE_PROJECT_ID: u64 = 1_012_168;

#[derive(Debug, Error)]
pub enum CleanroomRuleError {
    #[error("[cleanroom] config is only valid when pack.loader = \"cleanroom\"")]
    CleanroomConfigOnNonCleanroomLoader,
    #[error("[cleanroom].scalar_version must be 2 or 3, got {0}")]
    UnsupportedScalarVersion(u8),
}

pub fn validate_cleanroom_config(config: &Config) -> Result<(), CleanroomRuleError> {
    if config.cleanroom.is_some() && config.pack.loader != LoaderKind::Cleanroom {
        return Err(CleanroomRuleError::CleanroomConfigOnNonCleanroomLoader);
    }
    if let Some(cleanroom) = &config.cleanroom {
        validate_scalar_version(cleanroom.scalar_version)?;
    }
    Ok(())
}

pub fn cleanroom_dependency_supplements(
    config: &Config,
) -> Result<Vec<DependencyEntry>, CleanroomRuleError> {
    cleanroom_dependency_supplements_for(config, &[])
}

pub fn cleanroom_dependency_supplements_for(
    config: &Config,
    existing_entries: &[DependencyEntry],
) -> Result<Vec<DependencyEntry>, CleanroomRuleError> {
    let existing = existing_entries
        .iter()
        .map(|entry| ExistingDependency {
            project_id: entry.project_id,
            slug: entry.slug.clone(),
        })
        .collect::<Vec<_>>();
    cleanroom_dependency_supplements_for_existing(config, &existing)
}

fn cleanroom_dependency_supplements_for_existing(
    config: &Config,
    existing_entries: &[ExistingDependency],
) -> Result<Vec<DependencyEntry>, CleanroomRuleError> {
    validate_cleanroom_config(config)?;
    if config.pack.loader != LoaderKind::Cleanroom {
        return Ok(Vec::new());
    }

    let cleanroom = config.cleanroom.as_ref();
    let use_scalar = cleanroom.map(|value| value.use_scalar).unwrap_or(false);
    let scalar_version = cleanroom.map(|value| value.scalar_version).unwrap_or(2);
    let relauncher = cleanroom
        .map(|value| value.relauncher)
        .unwrap_or(CleanroomRelauncher::Unofficial);
    validate_scalar_version(scalar_version)?;

    let mut entries = Vec::new();

    if !contains_any_relauncher(existing_entries) {
        entries.push(relauncher_entry(relauncher));
    }

    if !contains_project_or_slug(existing_entries, &[FUGUE_PROJECT_ID], &[FUGUE_SLUG]) {
        entries.push(latest_mod(
            FUGUE_PROJECT_ID,
            FUGUE_SLUG,
            "Fugue",
            DependencySide::Common,
        ));
    }

    if use_scalar
        && !contains_project_or_slug(
            existing_entries,
            &[SCALAR_TWO_PROJECT_ID, SCALAR_THREE_PROJECT_ID],
            &["scalar"],
        )
    {
        entries.push(match scalar_version {
            2 => DependencyEntry {
                project_id: Some(SCALAR_TWO_PROJECT_ID),
                file_id: Some(SCALAR_TWO_FILE_ID),
                slug: Some("scalar".to_string()),
                loader: Some("forge".to_string()),
                side: DependencySide::Common,
                dependency_type: DependencyType::Mod,
                name: "Scalar".to_string(),
                authors: Vec::new(),
                required: true,
            },
            3 => latest_mod(
                SCALAR_THREE_PROJECT_ID,
                "scalar",
                "Scalar",
                DependencySide::Common,
            ),
            other => return Err(CleanroomRuleError::UnsupportedScalarVersion(other)),
        });
    }

    Ok(entries)
}

fn validate_scalar_version(version: u8) -> Result<(), CleanroomRuleError> {
    match version {
        2 | 3 => Ok(()),
        other => Err(CleanroomRuleError::UnsupportedScalarVersion(other)),
    }
}

fn latest_mod(project_id: u64, slug: &str, name: &str, side: DependencySide) -> DependencyEntry {
    DependencyEntry {
        project_id: Some(project_id),
        file_id: None,
        slug: Some(slug.to_string()),
        loader: Some("forge".to_string()),
        side,
        dependency_type: DependencyType::Mod,
        name: name.to_string(),
        authors: Vec::new(),
        required: true,
    }
}

fn relauncher_entry(relauncher: CleanroomRelauncher) -> DependencyEntry {
    match relauncher {
        CleanroomRelauncher::Unofficial => latest_mod(
            CLEANROOM_RELAUNCHER_UNOFFICIAL_PROJECT_ID,
            CLEANROOM_RELAUNCHER_UNOFFICIAL_SLUG,
            "Cleanroom Relauncher Unofficial",
            DependencySide::Client,
        ),
        CleanroomRelauncher::Original => latest_mod(
            CLEANROOM_RELAUNCHER_ORIGINAL_PROJECT_ID,
            CLEANROOM_RELAUNCHER_ORIGINAL_SLUG,
            "Cleanroom Relauncher",
            DependencySide::Client,
        ),
        CleanroomRelauncher::Improved => latest_mod(
            CLEANROOM_RELAUNCHER_IMPROVED_PROJECT_ID,
            CLEANROOM_RELAUNCHER_IMPROVED_SLUG,
            "Improved Cleanroom Relauncher",
            DependencySide::Client,
        ),
    }
}

fn contains_any_relauncher(entries: &[ExistingDependency]) -> bool {
    contains_project_or_slug(
        entries,
        &[
            CLEANROOM_RELAUNCHER_ORIGINAL_PROJECT_ID,
            CLEANROOM_RELAUNCHER_UNOFFICIAL_PROJECT_ID,
            CLEANROOM_RELAUNCHER_IMPROVED_PROJECT_ID,
        ],
        &[
            CLEANROOM_RELAUNCHER_ORIGINAL_SLUG,
            CLEANROOM_RELAUNCHER_UNOFFICIAL_SLUG,
            CLEANROOM_RELAUNCHER_IMPROVED_SLUG,
        ],
    )
}

fn contains_project_or_slug(
    entries: &[ExistingDependency],
    project_ids: &[u64],
    slugs: &[&str],
) -> bool {
    entries.iter().any(|entry| {
        entry
            .project_id
            .is_some_and(|project_id| project_ids.contains(&project_id))
            || entry.slug.as_deref().is_some_and(|slug| {
                slugs
                    .iter()
                    .any(|candidate| slug.eq_ignore_ascii_case(candidate))
            })
    })
}

struct ExistingDependency {
    project_id: Option<u64>,
    slug: Option<String>,
}
