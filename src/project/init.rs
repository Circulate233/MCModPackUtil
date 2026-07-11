use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use rayon::prelude::*;
use thiserror::Error;
use zip::ZipArchive;

use crate::config::{Config, ConfigError};
use crate::curseforge::model::{CfFile, CfProject};
use crate::curseforge::resolver::{CachedCurseForgeApi, CurseForgeApi, ResolveError};
use crate::dependency::{DependencyEntry, DependencyError, DependencySide, DependencyType};
use crate::manifest::curseforge_pack::{ImportedManifest, ManifestError};
use crate::project::cleanroom::CleanroomRuleError;

const SIDE_INFERENCE_REPORT: &str = "side-inference-report.txt";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InitProgress {
    OpeningZip {
        path: String,
    },
    ReadingManifest,
    ManifestLoaded {
        name: String,
        version: String,
        file_count: usize,
    },
    ResolvedFile {
        completed: usize,
        total: usize,
        project_id: u64,
        file_id: u64,
    },
    WritingProjectFiles {
        output_dir: String,
    },
    ExtractingOverrides {
        overrides_dir: String,
    },
    Finished {
        output_dir: String,
    },
}

#[derive(Debug, Error)]
pub enum InitError {
    #[error("failed to open input zip '{path}': {source}")]
    OpenZip {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to read input zip '{path}': {source}")]
    ReadZip {
        path: String,
        #[source]
        source: zip::result::ZipError,
    },
    #[error("input zip is missing manifest.json")]
    MissingManifest,
    #[error(transparent)]
    Manifest(#[from] ManifestError),
    #[error(transparent)]
    Config(#[from] ConfigError),
    #[error("failed to create directory '{path}': {source}")]
    CreateDir {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to write file '{path}': {source}")]
    WriteFile {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to serialize TOML config: {0}")]
    SerializeToml(#[from] toml::ser::Error),
    #[error("failed to serialize JSON list: {0}")]
    SerializeJson(#[from] serde_json::Error),
    #[error(transparent)]
    Cleanroom(#[from] CleanroomRuleError),
    #[error(transparent)]
    Dependency(#[from] DependencyError),
    #[error(transparent)]
    Resolve(#[from] ResolveError),
}

pub fn initialize_from_zip(
    zip_path: impl AsRef<Path>,
    output_dir: impl AsRef<Path>,
    language: impl Into<String>,
) -> Result<(), InitError> {
    initialize_from_zip_with_progress(zip_path, output_dir, language, &mut |_| {})
}

pub fn initialize_from_zip_with_progress<F>(
    zip_path: impl AsRef<Path>,
    output_dir: impl AsRef<Path>,
    language: impl Into<String>,
    progress: &mut F,
) -> Result<(), InitError>
where
    F: FnMut(&InitProgress) + Send,
{
    initialize_from_zip_resolving(
        zip_path,
        output_dir,
        language.into(),
        None::<&NoopApi>,
        progress,
    )
}

pub fn initialize_from_zip_with_api<A: CurseForgeApi + Sync>(
    zip_path: impl AsRef<Path>,
    output_dir: impl AsRef<Path>,
    language: impl Into<String>,
    api: &A,
) -> Result<(), InitError> {
    initialize_from_zip_resolving(
        zip_path,
        output_dir,
        language.into(),
        Some(api),
        &mut |_| {},
    )
}

pub fn initialize_from_zip_with_api_and_progress<A, F>(
    zip_path: impl AsRef<Path>,
    output_dir: impl AsRef<Path>,
    language: impl Into<String>,
    api: &A,
    progress: &mut F,
) -> Result<(), InitError>
where
    A: CurseForgeApi + Sync,
    F: FnMut(&InitProgress) + Send,
{
    initialize_from_zip_resolving(zip_path, output_dir, language.into(), Some(api), progress)
}

fn initialize_from_zip_resolving<A, F>(
    zip_path: impl AsRef<Path>,
    output_dir: impl AsRef<Path>,
    language: String,
    api: Option<&A>,
    progress: &mut F,
) -> Result<(), InitError>
where
    A: CurseForgeApi + Sync,
    F: FnMut(&InitProgress) + Send,
{
    let zip_path = zip_path.as_ref();
    let output_dir = output_dir.as_ref();
    progress(&InitProgress::OpeningZip {
        path: zip_path.display().to_string(),
    });
    let file = File::open(zip_path).map_err(|source| InitError::OpenZip {
        path: zip_path.display().to_string(),
        source,
    })?;
    let mut archive = ZipArchive::new(file).map_err(|source| InitError::ReadZip {
        path: zip_path.display().to_string(),
        source,
    })?;

    progress(&InitProgress::ReadingManifest);
    let manifest = read_manifest(&mut archive)?;
    progress(&InitProgress::ManifestLoaded {
        name: manifest.name.clone(),
        version: manifest.version.clone(),
        file_count: manifest.files.len(),
    });
    let config = Config::from_imported_manifest(&manifest, language.clone());
    let resolved = match api {
        Some(api) => {
            let cached_api = CachedCurseForgeApi::new(api);
            resolve_manifest_entries(&manifest, &cached_api, progress)?
        }
        None if !manifest.files.is_empty() => return Err(ResolveError::MissingApiKey.into()),
        None => ResolvedManifestEntries::default(),
    };
    let entries = resolved.entries;

    progress(&InitProgress::WritingProjectFiles {
        output_dir: output_dir.display().to_string(),
    });
    create_dir(output_dir)?;
    write_text(
        output_dir.join("config.toml"),
        &toml::to_string_pretty(&config)?,
    )?;
    write_list(
        output_dir.join("common.json"),
        &entries_for_side(&entries, DependencySide::Common),
    )?;
    write_list(
        output_dir.join("client.json"),
        &entries_for_side(&entries, DependencySide::Client),
    )?;
    write_list(
        output_dir.join("server.json"),
        &entries_for_side(&entries, DependencySide::Server),
    )?;
    write_list(
        output_dir.join("resourcepacks.json"),
        &entries_for_type(
            &entries,
            &[DependencyType::ResourcePack, DependencyType::ShaderPack],
        ),
    )?;
    write_text(
        output_dir.join(SIDE_INFERENCE_REPORT),
        &render_side_inference_report(&resolved.unknown_side_entries, &language),
    )?;

    let overrides_root = output_dir.join("overrides");
    create_dir(overrides_root.join("common"))?;
    create_dir(overrides_root.join("client"))?;
    create_dir(overrides_root.join("server"))?;
    progress(&InitProgress::ExtractingOverrides {
        overrides_dir: manifest.overrides.clone(),
    });
    extract_overrides(
        &mut archive,
        &manifest.overrides,
        &overrides_root.join("common"),
    )?;

    progress(&InitProgress::Finished {
        output_dir: output_dir.display().to_string(),
    });
    Ok(())
}

struct NoopApi;

impl CurseForgeApi for NoopApi {
    fn get_project(
        &self,
        project_id: u64,
    ) -> Result<crate::curseforge::model::CfProject, ResolveError> {
        Err(ResolveError::Api(format!(
            "CurseForge API is not configured for project {project_id}"
        )))
    }

    fn get_file(
        &self,
        project_id: u64,
        file_id: u64,
    ) -> Result<crate::curseforge::model::CfFile, ResolveError> {
        Err(ResolveError::Api(format!(
            "CurseForge API is not configured for file {project_id}/{file_id}"
        )))
    }

    fn find_project_by_slug(
        &self,
        slug: &str,
    ) -> Result<crate::curseforge::model::CfProject, ResolveError> {
        Err(ResolveError::Api(format!(
            "CurseForge API is not configured for slug {slug}"
        )))
    }

    fn list_files(
        &self,
        project_id: u64,
        _minecraft_version: &str,
        _loader: &str,
    ) -> Result<Vec<crate::curseforge::model::CfFile>, ResolveError> {
        Err(ResolveError::Api(format!(
            "CurseForge API is not configured for files of project {project_id}"
        )))
    }
}

#[derive(Debug, Default)]
struct ResolvedManifestEntries {
    entries: Vec<DependencyEntry>,
    unknown_side_entries: Vec<UnknownSideEntry>,
}

#[derive(Debug)]
struct ResolvedManifestEntry {
    index: usize,
    entry: DependencyEntry,
    unknown_side_entry: Option<UnknownSideEntry>,
}

#[derive(Debug)]
struct UnknownSideEntry {
    project_id: u64,
    file_id: u64,
    name: String,
    slug: String,
    file_name: String,
}

fn resolve_manifest_entries<A, F>(
    manifest: &ImportedManifest,
    api: &A,
    progress: &mut F,
) -> Result<ResolvedManifestEntries, InitError>
where
    A: CurseForgeApi + Sync,
    F: FnMut(&InitProgress) + Send,
{
    let total = manifest.files.len();
    let completed = Mutex::new(0_usize);
    let progress = Mutex::new(progress);
    let mut resolved_entries = manifest
        .files
        .par_iter()
        .enumerate()
        .map(|(index, manifest_file)| {
            let (project, file) = rayon::join(
                || api.get_project(manifest_file.project_id),
                || api.get_file(manifest_file.project_id, manifest_file.file_id),
            );
            let project = project?;
            let file = file?;
            let unknown_side_entry;
            let mut entry = match DependencyEntry::from_curseforge(&project, &file) {
                Ok(entry) => {
                    unknown_side_entry = None;
                    entry
                }
                Err(DependencyError::MissingSide { .. }) => {
                    if let Some(side) = infer_side_from_project_files(manifest, api, &file)? {
                        unknown_side_entry = None;
                        DependencyEntry::from_curseforge_with_side(&project, &file, side)?
                    } else {
                        unknown_side_entry =
                            Some(UnknownSideEntry::from_curseforge(&project, &file));
                        DependencyEntry::from_curseforge_with_side(
                            &project,
                            &file,
                            DependencySide::Common,
                        )?
                    }
                }
                Err(err) => return Err(err.into()),
            };
            entry.required = manifest_file.required;

            let completed = {
                let mut completed = completed
                    .lock()
                    .expect("init completed counter mutex poisoned");
                *completed += 1;
                *completed
            };
            {
                let mut progress = progress.lock().expect("init progress mutex poisoned");
                progress(&InitProgress::ResolvedFile {
                    completed,
                    total,
                    project_id: manifest_file.project_id,
                    file_id: manifest_file.file_id,
                });
            }

            Ok(ResolvedManifestEntry {
                index,
                entry,
                unknown_side_entry,
            })
        })
        .collect::<Result<Vec<_>, InitError>>()?;

    resolved_entries.sort_by_key(|entry| entry.index);
    Ok(ResolvedManifestEntries {
        entries: resolved_entries
            .iter()
            .map(|entry| entry.entry.clone())
            .collect(),
        unknown_side_entries: resolved_entries
            .into_iter()
            .filter_map(|entry| entry.unknown_side_entry)
            .collect(),
    })
}

fn infer_side_from_project_files<A: CurseForgeApi>(
    manifest: &ImportedManifest,
    api: &A,
    locked_file: &CfFile,
) -> Result<Option<DependencySide>, ResolveError> {
    let loader = locked_file.loader().unwrap_or_else(|| {
        let (loader_name, _) = manifest.primary_loader_parts();
        loader_name
    });
    let files = api.list_files(locked_file.mod_id, &manifest.minecraft.version, &loader)?;
    if let Some(side) = infer_side_from_files(&files) {
        return Ok(Some(side));
    }
    let project_files = api.list_project_files(locked_file.mod_id)?;
    Ok(infer_side_from_files(&project_files))
}

fn infer_side_from_files(files: &[CfFile]) -> Option<DependencySide> {
    let has_client = files.iter().any(CfFile::has_client_tag);
    let has_server = files.iter().any(CfFile::has_server_tag);
    match (has_client, has_server) {
        (true, true) => Some(DependencySide::Common),
        (true, false) => Some(DependencySide::Client),
        (false, true) => Some(DependencySide::Server),
        (false, false) => None,
    }
}

impl UnknownSideEntry {
    fn from_curseforge(project: &CfProject, file: &CfFile) -> Self {
        Self {
            project_id: project.id,
            file_id: file.id,
            name: project.name.clone(),
            slug: project.slug.clone(),
            file_name: file.file_name.clone(),
        }
    }
}

fn render_side_inference_report(entries: &[UnknownSideEntry], language: &str) -> String {
    let mut report = String::new();
    if language.to_ascii_lowercase().starts_with("zh") {
        report.push_str("# 端侧自动推测报告\n\n");
        if entries.is_empty() {
            report.push_str("所有依赖均已自动推测端侧。\n");
            return report;
        }
        report.push_str(
            "以下 CurseForge 文件缺少 Client/Server 端侧信息，已默认写入 common.json（双端）：\n\n",
        );
    } else {
        report.push_str("# Side inference report\n\n");
        if entries.is_empty() {
            report.push_str("All dependencies had enough side metadata for automatic inference.\n");
            return report;
        }
        report.push_str(
            "The following CurseForge files did not expose Client/Server side metadata and were written to common.json by default:\n\n",
        );
    }

    for entry in entries {
        report.push_str(&format!(
            "- {} ({}) {}/{} {} -> common\n",
            entry.name, entry.slug, entry.project_id, entry.file_id, entry.file_name
        ));
    }
    report
}

fn read_manifest(archive: &mut ZipArchive<File>) -> Result<ImportedManifest, InitError> {
    let mut manifest_file = archive
        .by_name("manifest.json")
        .map_err(|_| InitError::MissingManifest)?;
    let mut bytes = Vec::new();
    manifest_file
        .read_to_end(&mut bytes)
        .map_err(|source| InitError::OpenZip {
            path: "manifest.json".to_string(),
            source,
        })?;
    ImportedManifest::from_slice(&bytes).map_err(Into::into)
}

fn extract_overrides(
    archive: &mut ZipArchive<File>,
    overrides_dir: &str,
    output_common: &Path,
) -> Result<(), InitError> {
    let prefix = format!("{}/", overrides_dir.trim_matches('/'));
    for index in 0..archive.len() {
        let mut entry = archive
            .by_index(index)
            .map_err(|source| InitError::ReadZip {
                path: format!("entry #{index}"),
                source,
            })?;
        let name = entry.name().replace('\\', "/");
        if !name.starts_with(&prefix) || name == prefix {
            continue;
        }
        let relative = &name[prefix.len()..];
        let target = safe_join(output_common, relative);
        if entry.is_dir() {
            create_dir(&target)?;
            continue;
        }
        if let Some(parent) = target.parent() {
            create_dir(parent)?;
        }
        let mut bytes = Vec::new();
        entry
            .read_to_end(&mut bytes)
            .map_err(|source| InitError::OpenZip {
                path: name.clone(),
                source,
            })?;
        write_bytes(target, &bytes)?;
    }
    Ok(())
}

fn safe_join(base: &Path, relative: &str) -> PathBuf {
    let mut path = base.to_path_buf();
    for component in relative.split('/') {
        if component.is_empty() || component == "." || component == ".." {
            continue;
        }
        path.push(component);
    }
    path
}

fn write_list(path: impl AsRef<Path>, entries: &[DependencyEntry]) -> Result<(), InitError> {
    let text = format!("{}\n", serde_json::to_string_pretty(&entries)?);
    write_text(path, &text)
}

fn entries_for_side(entries: &[DependencyEntry], side: DependencySide) -> Vec<DependencyEntry> {
    entries
        .iter()
        .filter(|entry| entry.side == side)
        .cloned()
        .collect()
}

fn entries_for_type(entries: &[DependencyEntry], types: &[DependencyType]) -> Vec<DependencyEntry> {
    entries
        .iter()
        .filter(|entry| types.contains(&entry.dependency_type))
        .cloned()
        .collect()
}

fn create_dir(path: impl AsRef<Path>) -> Result<(), InitError> {
    let path = path.as_ref();
    fs::create_dir_all(path).map_err(|source| InitError::CreateDir {
        path: path.display().to_string(),
        source,
    })
}

fn write_text(path: impl AsRef<Path>, text: &str) -> Result<(), InitError> {
    write_bytes(path, text.as_bytes())
}

fn write_bytes(path: impl AsRef<Path>, bytes: &[u8]) -> Result<(), InitError> {
    let path = path.as_ref();
    let mut file = File::create(path).map_err(|source| InitError::WriteFile {
        path: path.display().to_string(),
        source,
    })?;
    file.write_all(bytes)
        .map_err(|source| InitError::WriteFile {
            path: path.display().to_string(),
            source,
        })
}
