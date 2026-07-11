use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use rayon::prelude::*;
use thiserror::Error;
use walkdir::WalkDir;
use zip::write::SimpleFileOptions;

use crate::config::{Config, ConfigError};
use crate::curseforge::client::CurseForgeClient;
use crate::curseforge::model::CfFile;
use crate::curseforge::resolver::{
    ApiKeyResolver, CachedCurseForgeApi, CandidateSelector, CurseForgeApi, ResolveError,
};
use crate::dependency::{DependencyEntry, DependencyError, DependencySide, DependencyType};
use crate::manifest::generated::{
    GeneratedManifestError, generate_modlist_html, generate_pack_manifest,
};
use crate::project::cleanroom::{CleanroomRuleError, cleanroom_dependency_supplements_for};
use crate::server::files::{ServerFile, ServerFilesManifest, ServerJavaConfig};
use crate::server::loader::{LoaderPlanError, plan_loader_install};
use crate::server::scripts::{render_download_ps1, render_start_bat, render_start_sh};
use crate::zip::optimize::{minify_json_bytes, optimize_png_bytes};

const TEST_BUILD_API_KEY: &str = "test-build-api-key";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuildOutputs {
    pub client_zip: PathBuf,
    pub server_zip: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuildValidationReport {
    pub client_manifest_entries: usize,
    pub server_downloads: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BuildProgress {
    ReadingProject {
        project_dir: String,
    },
    DependencyListsLoaded {
        common: usize,
        client: usize,
        server: usize,
        resourcepacks: usize,
    },
    CleanroomSupplementsApplied {
        added: usize,
    },
    ResolvingDependencies {
        total: usize,
    },
    ResolvedDependency {
        completed: usize,
        total: usize,
        project_id: Option<u64>,
        file_id: Option<u64>,
        slug: Option<String>,
    },
    PreparingOutput {
        output_dir: String,
    },
    WritingClientZip {
        path: String,
    },
    WritingServerZip {
        path: String,
    },
    Finished {
        client_zip: String,
        server_zip: String,
    },
}

#[derive(Debug, Error)]
pub enum BuildError {
    #[error(transparent)]
    Config(#[from] ConfigError),
    #[error("failed to read file '{path}': {source}")]
    ReadFile {
        path: String,
        #[source]
        source: std::io::Error,
    },
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
    #[error("failed to read dependency list '{path}': {source}")]
    ReadDependencyList {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to parse dependency list '{path}': {source}")]
    ParseDependencyList {
        path: String,
        #[source]
        source: serde_json::Error,
    },
    #[error("failed to serialize JSON: {0}")]
    SerializeJson(#[from] serde_json::Error),
    #[error(transparent)]
    GeneratedManifest(#[from] GeneratedManifestError),
    #[error(transparent)]
    Resolve(#[from] ResolveError),
    #[error(transparent)]
    Dependency(#[from] DependencyError),
    #[error(transparent)]
    Cleanroom(#[from] CleanroomRuleError),
    #[error(transparent)]
    Loader(#[from] LoaderPlanError),
    #[error("internal dependency resolution bucket '{bucket}' missed index {index}")]
    ResolutionBucket { bucket: &'static str, index: usize },
    #[error("CurseForge file {project_id}/{file_id} has no SHA1 hash")]
    MissingSha1 { project_id: u64, file_id: u64 },
    #[error("CurseForge file {project_id}/{file_id} has no download URL")]
    MissingDownloadUrl { project_id: u64, file_id: u64 },
    #[error("failed to write zip '{path}': {source}")]
    Zip {
        path: String,
        #[source]
        source: zip::result::ZipError,
    },
    #[error("failed to optimize JSON '{path}': {source}")]
    OptimizeJson {
        path: String,
        #[source]
        source: serde_json::Error,
    },
    #[error("failed to optimize PNG '{path}': {source}")]
    OptimizePng {
        path: String,
        #[source]
        source: oxipng::PngError,
    },
    #[error("failed to walk directory '{path}': {source}")]
    WalkDir {
        path: String,
        #[source]
        source: walkdir::Error,
    },
}

#[derive(Debug, Clone)]
struct ResolvedEntry {
    entry: DependencyEntry,
    file: CfFile,
}

#[derive(Debug, Clone)]
pub struct RuntimeResolvedEntry {
    pub entry: DependencyEntry,
    pub file: CfFile,
}

#[derive(Debug, Clone)]
pub struct RuntimeResolvedLists {
    pub common: Vec<RuntimeResolvedEntry>,
    pub client: Vec<RuntimeResolvedEntry>,
    pub server: Vec<RuntimeResolvedEntry>,
    pub resourcepacks: Vec<RuntimeResolvedEntry>,
}

#[derive(Debug, Clone, Copy)]
enum ListBucket {
    Common,
    Client,
    Server,
    Resourcepacks,
}

#[derive(Debug, Clone)]
struct QueuedEntry {
    bucket: ListBucket,
    index: usize,
    entry: DependencyEntry,
}

#[derive(Debug, Clone)]
struct ResolvedQueuedEntry {
    bucket: ListBucket,
    index: usize,
    resolved: ResolvedEntry,
}

pub fn build_project(project_dir: impl AsRef<Path>) -> Result<BuildOutputs, BuildError> {
    let project_dir = project_dir.as_ref();
    let config = Config::from_path(project_dir.join("config.toml"))?;
    let env_api_key = ApiKeyResolver::env_key();
    let builtin_api_key = ApiKeyResolver::builtin_key();
    let api_key = ApiKeyResolver::resolve(
        None,
        env_api_key.as_deref(),
        builtin_api_key.as_deref().unwrap_or(""),
    )?;
    let config_proxy = proxy_from_config(&config)?;
    let api = CurseForgeClient::with_proxy(api_key.clone(), config_proxy.as_deref())?;
    build_project_with_config_and_api(project_dir, config, &api, &api_key, &mut |_| {})
}

pub fn build_project_with_api_key(
    project_dir: impl AsRef<Path>,
    api_key: String,
    proxy: Option<&str>,
) -> Result<BuildOutputs, BuildError> {
    let project_dir = project_dir.as_ref();
    let config = Config::from_path(project_dir.join("config.toml"))?;
    let config_proxy = proxy_from_config(&config)?;
    let api = CurseForgeClient::with_proxy(api_key.clone(), proxy.or(config_proxy.as_deref()))?;
    build_project_with_config_and_api(project_dir, config, &api, &api_key, &mut |_| {})
}

pub fn build_project_with_api_key_and_progress<F>(
    project_dir: impl AsRef<Path>,
    api_key: String,
    proxy: Option<&str>,
    progress: &mut F,
) -> Result<BuildOutputs, BuildError>
where
    F: FnMut(&BuildProgress) + Send,
{
    let project_dir = project_dir.as_ref();
    let config = Config::from_path(project_dir.join("config.toml"))?;
    let config_proxy = proxy_from_config(&config)?;
    let api = CurseForgeClient::with_proxy(api_key.clone(), proxy.or(config_proxy.as_deref()))?;
    build_project_with_config_and_api(project_dir, config, &api, &api_key, progress)
}

fn proxy_from_config(config: &Config) -> Result<Option<String>, BuildError> {
    config.network.proxy_url().map_err(Into::into)
}

pub fn build_project_with_api<A: CurseForgeApi + Sync>(
    project_dir: impl AsRef<Path>,
    api: &A,
) -> Result<BuildOutputs, BuildError> {
    let project_dir = project_dir.as_ref();
    let config = Config::from_path(project_dir.join("config.toml"))?;
    build_project_with_config_and_api(project_dir, config, api, TEST_BUILD_API_KEY, &mut |_| {})
}

pub fn build_project_with_api_and_progress<A, F>(
    project_dir: impl AsRef<Path>,
    api: &A,
    progress: &mut F,
) -> Result<BuildOutputs, BuildError>
where
    A: CurseForgeApi + Sync,
    F: FnMut(&BuildProgress) + Send,
{
    let project_dir = project_dir.as_ref();
    let config = Config::from_path(project_dir.join("config.toml"))?;
    build_project_with_config_and_api(project_dir, config, api, TEST_BUILD_API_KEY, progress)
}

pub fn validate_project_with_api<A: CurseForgeApi + Sync>(
    project_dir: impl AsRef<Path>,
    api: &A,
) -> Result<BuildValidationReport, BuildError> {
    validate_project_with_api_and_progress(project_dir, api, &mut |_| {})
}

pub fn validate_project_with_api_and_progress<A, F>(
    project_dir: impl AsRef<Path>,
    api: &A,
    progress: &mut F,
) -> Result<BuildValidationReport, BuildError>
where
    A: CurseForgeApi + Sync,
    F: FnMut(&BuildProgress) + Send,
{
    let project_dir = project_dir.as_ref();
    progress(&BuildProgress::ReadingProject {
        project_dir: project_dir.display().to_string(),
    });
    let config = Config::from_path(project_dir.join("config.toml"))?;
    let lists = ProjectLists::read(project_dir)?;
    let before_supplements = lists.total_len();
    progress(&BuildProgress::DependencyListsLoaded {
        common: lists.common.len(),
        client: lists.client.len(),
        server: lists.server.len(),
        resourcepacks: lists.resourcepacks.len(),
    });
    let lists = lists.with_cleanroom_supplements(&config)?;
    progress(&BuildProgress::CleanroomSupplementsApplied {
        added: lists.total_len().saturating_sub(before_supplements),
    });
    let resolved = ResolvedLists::resolve(&config, api, &lists, progress)?;
    let manifest = server_files_manifest(&config, &resolved, TEST_BUILD_API_KEY)?;
    Ok(BuildValidationReport {
        client_manifest_entries: resolved.client_manifest_entries().len(),
        server_downloads: manifest.files.len(),
    })
}

pub fn resolve_project_for_runtime_with_api<A: CurseForgeApi + Sync>(
    project_dir: impl AsRef<Path>,
    api: &A,
) -> Result<(Config, RuntimeResolvedLists), BuildError> {
    let project_dir = project_dir.as_ref();
    let config = Config::from_path(project_dir.join("config.toml"))?;
    let lists = ProjectLists::read(project_dir)?;
    let lists = lists.with_cleanroom_runtime_supplements(&config)?;
    let resolved = ResolvedLists::resolve_without_download_url_check(&config, api, &lists)?;
    Ok((config, resolved.into_runtime()))
}

fn build_project_with_config_and_api<A: CurseForgeApi + Sync>(
    project_dir: &Path,
    config: Config,
    api: &A,
    server_api_key: &str,
    progress: &mut (impl FnMut(&BuildProgress) + Send),
) -> Result<BuildOutputs, BuildError> {
    progress(&BuildProgress::ReadingProject {
        project_dir: project_dir.display().to_string(),
    });
    let lists = ProjectLists::read(project_dir)?;
    let before_supplements = lists.total_len();
    progress(&BuildProgress::DependencyListsLoaded {
        common: lists.common.len(),
        client: lists.client.len(),
        server: lists.server.len(),
        resourcepacks: lists.resourcepacks.len(),
    });
    let lists = lists.with_cleanroom_supplements(&config)?;
    progress(&BuildProgress::CleanroomSupplementsApplied {
        added: lists.total_len().saturating_sub(before_supplements),
    });
    let resolved = ResolvedLists::resolve(&config, api, &lists, progress)?;

    let output_dir = project_dir.join(&config.build.output_dir);
    progress(&BuildProgress::PreparingOutput {
        output_dir: output_dir.display().to_string(),
    });
    create_dir(&output_dir)?;
    let base_name = format!("{}-{}", config.pack.name, config.pack.version);
    let client_zip = output_dir.join(format!("{}.zip", config.pack.name));
    let server_zip = output_dir.join(format!("{base_name}-server.zip"));

    progress(&BuildProgress::WritingClientZip {
        path: client_zip.display().to_string(),
    });
    write_client_zip(project_dir, &client_zip, &config, &resolved)?;
    progress(&BuildProgress::WritingServerZip {
        path: server_zip.display().to_string(),
    });
    write_server_zip(project_dir, &server_zip, &config, &resolved, server_api_key)?;

    progress(&BuildProgress::Finished {
        client_zip: client_zip.display().to_string(),
        server_zip: server_zip.display().to_string(),
    });
    Ok(BuildOutputs {
        client_zip,
        server_zip,
    })
}

#[derive(Debug, Clone)]
struct ProjectLists {
    common: Vec<DependencyEntry>,
    client: Vec<DependencyEntry>,
    server: Vec<DependencyEntry>,
    resourcepacks: Vec<DependencyEntry>,
}

impl ProjectLists {
    fn read(project_dir: &Path) -> Result<Self, BuildError> {
        Ok(Self {
            common: read_dependency_list(project_dir.join("common.json"), DependencySide::Common)?,
            client: read_dependency_list(project_dir.join("client.json"), DependencySide::Client)?,
            server: read_dependency_list(project_dir.join("server.json"), DependencySide::Server)?,
            resourcepacks: read_dependency_list(
                project_dir.join("resourcepacks.json"),
                DependencySide::Resourcepack,
            )?,
        })
    }

    fn all_entries(&self) -> Vec<DependencyEntry> {
        self.common
            .iter()
            .chain(&self.client)
            .chain(&self.server)
            .chain(&self.resourcepacks)
            .cloned()
            .collect()
    }

    fn total_len(&self) -> usize {
        self.common.len() + self.client.len() + self.server.len() + self.resourcepacks.len()
    }

    fn with_cleanroom_supplements(mut self, config: &Config) -> Result<Self, BuildError> {
        for entry in cleanroom_dependency_supplements_for(config, &self.all_entries())? {
            match entry.side {
                DependencySide::Common => self.common.push(entry),
                DependencySide::Client => self.client.push(entry),
                DependencySide::Server => self.server.push(entry),
                DependencySide::Resourcepack => self.resourcepacks.push(entry),
            }
        }
        Ok(self)
    }

    fn with_cleanroom_runtime_supplements(mut self, config: &Config) -> Result<Self, BuildError> {
        for entry in cleanroom_dependency_supplements_for(config, &self.all_entries())?
            .into_iter()
            .filter(|entry| !is_cleanroom_relauncher_entry(entry))
        {
            match entry.side {
                DependencySide::Common => self.common.push(entry),
                DependencySide::Client => self.client.push(entry),
                DependencySide::Server => self.server.push(entry),
                DependencySide::Resourcepack => self.resourcepacks.push(entry),
            }
        }
        Ok(self)
    }
}

#[derive(Debug, Clone)]
struct ResolvedLists {
    common: Vec<ResolvedEntry>,
    client: Vec<ResolvedEntry>,
    server: Vec<ResolvedEntry>,
    resourcepacks: Vec<ResolvedEntry>,
}

impl ResolvedLists {
    fn resolve<A: CurseForgeApi + Sync>(
        config: &Config,
        api: &A,
        lists: &ProjectLists,
        progress: &mut (impl FnMut(&BuildProgress) + Send),
    ) -> Result<Self, BuildError> {
        Self::resolve_with_options(config, api, lists, progress)
    }

    fn resolve_without_download_url_check<A: CurseForgeApi + Sync>(
        config: &Config,
        api: &A,
        lists: &ProjectLists,
    ) -> Result<Self, BuildError> {
        Self::resolve_with_options(config, api, lists, &mut |_| {})
    }

    fn resolve_with_options<A: CurseForgeApi + Sync>(
        config: &Config,
        api: &A,
        lists: &ProjectLists,
        progress: &mut (impl FnMut(&BuildProgress) + Send),
    ) -> Result<Self, BuildError> {
        let api = CachedCurseForgeApi::new(api);
        let queued_entries = queued_entries(lists);
        let total = queued_entries.len();
        progress(&BuildProgress::ResolvingDependencies { total });
        let completed = Mutex::new(0_usize);
        let progress = Mutex::new(progress);
        let resolved_entries = queued_entries
            .par_iter()
            .map(|queued| {
                let resolved = resolve_entry(config, &api, &queued.entry)?;
                let completed = {
                    let mut completed = completed
                        .lock()
                        .expect("build completed counter mutex poisoned");
                    *completed += 1;
                    *completed
                };
                {
                    let mut progress = progress.lock().expect("build progress mutex poisoned");
                    progress(&BuildProgress::ResolvedDependency {
                        completed,
                        total,
                        project_id: resolved.entry.project_id,
                        file_id: resolved.entry.file_id,
                        slug: resolved.entry.slug.clone(),
                    });
                }
                Ok(ResolvedQueuedEntry {
                    bucket: queued.bucket,
                    index: queued.index,
                    resolved,
                })
            })
            .collect::<Result<Vec<_>, BuildError>>()?;

        let mut common = vec![None; lists.common.len()];
        let mut client = vec![None; lists.client.len()];
        let mut server = vec![None; lists.server.len()];
        let mut resourcepacks = vec![None; lists.resourcepacks.len()];

        for resolved in resolved_entries {
            match resolved.bucket {
                ListBucket::Common => common[resolved.index] = Some(resolved.resolved),
                ListBucket::Client => client[resolved.index] = Some(resolved.resolved),
                ListBucket::Server => server[resolved.index] = Some(resolved.resolved),
                ListBucket::Resourcepacks => {
                    resourcepacks[resolved.index] = Some(resolved.resolved)
                }
            }
        }

        Ok(Self {
            common: unwrap_resolved_bucket("common", common)?,
            client: unwrap_resolved_bucket("client", client)?,
            server: unwrap_resolved_bucket("server", server)?,
            resourcepacks: unwrap_resolved_bucket("resourcepacks", resourcepacks)?,
        })
    }

    fn client_manifest_entries(&self) -> Vec<DependencyEntry> {
        self.common
            .iter()
            .chain(&self.client)
            .chain(&self.resourcepacks)
            .map(|resolved| resolved.entry.clone())
            .collect()
    }

    fn server_downloads(&self) -> Vec<&ResolvedEntry> {
        self.common
            .iter()
            .chain(&self.server)
            .chain(
                self.resourcepacks.iter().filter(|resolved| {
                    resolved.entry.dependency_type == DependencyType::ResourcePack
                }),
            )
            .collect()
    }

    fn into_runtime(self) -> RuntimeResolvedLists {
        RuntimeResolvedLists {
            common: self.common.into_iter().map(Into::into).collect(),
            client: self.client.into_iter().map(Into::into).collect(),
            server: self.server.into_iter().map(Into::into).collect(),
            resourcepacks: self.resourcepacks.into_iter().map(Into::into).collect(),
        }
    }
}

impl From<ResolvedEntry> for RuntimeResolvedEntry {
    fn from(value: ResolvedEntry) -> Self {
        Self {
            entry: value.entry,
            file: value.file,
        }
    }
}

fn is_cleanroom_relauncher_entry(entry: &DependencyEntry) -> bool {
    const RELAUNCHER_PROJECTS: &[u64] = &[1_214_490, 1_214_906, 1_336_201];
    const RELAUNCHER_SLUGS: &[&str] = &[
        "cleanroom-relauncher",
        "cleanroom-relauncher-unofficial",
        "improved-cleanroom-relauncher",
    ];
    entry
        .project_id
        .is_some_and(|project_id| RELAUNCHER_PROJECTS.contains(&project_id))
        || entry.slug.as_deref().is_some_and(|slug| {
            RELAUNCHER_SLUGS
                .iter()
                .any(|candidate| slug.eq_ignore_ascii_case(candidate))
        })
}

fn queued_entries(lists: &ProjectLists) -> Vec<QueuedEntry> {
    lists
        .common
        .iter()
        .cloned()
        .enumerate()
        .map(|(index, entry)| QueuedEntry {
            bucket: ListBucket::Common,
            index,
            entry,
        })
        .chain(
            lists
                .client
                .iter()
                .cloned()
                .enumerate()
                .map(|(index, entry)| QueuedEntry {
                    bucket: ListBucket::Client,
                    index,
                    entry,
                }),
        )
        .chain(
            lists
                .server
                .iter()
                .cloned()
                .enumerate()
                .map(|(index, entry)| QueuedEntry {
                    bucket: ListBucket::Server,
                    index,
                    entry,
                }),
        )
        .chain(
            lists
                .resourcepacks
                .iter()
                .cloned()
                .enumerate()
                .map(|(index, entry)| QueuedEntry {
                    bucket: ListBucket::Resourcepacks,
                    index,
                    entry,
                }),
        )
        .collect()
}

fn unwrap_resolved_bucket(
    bucket: &'static str,
    entries: Vec<Option<ResolvedEntry>>,
) -> Result<Vec<ResolvedEntry>, BuildError> {
    entries
        .into_iter()
        .enumerate()
        .map(|(index, entry)| entry.ok_or(BuildError::ResolutionBucket { bucket, index }))
        .collect()
}

fn resolve_entry<A: CurseForgeApi + Sync>(
    config: &Config,
    api: &A,
    entry: &DependencyEntry,
) -> Result<ResolvedEntry, BuildError> {
    let loader = entry.loader.as_deref().unwrap_or("");
    let (project, file) = match (entry.project_id, entry.file_id) {
        (Some(project_id), Some(file_id)) => {
            let (project, file) = rayon::join(
                || api.get_project(project_id),
                || api.get_file(project_id, file_id),
            );
            let project = project?;
            let file = file?;
            (project, file)
        }
        (Some(project_id), None) => {
            let project = api.get_project(project_id)?;
            let files = api.list_files(project_id, &config.pack.minecraft_version, loader)?;
            let file = CandidateSelector::select_latest(&files)?;
            (project, file)
        }
        (None, _) => {
            let slug = entry
                .slug
                .as_deref()
                .ok_or_else(|| ResolveError::MissingSlug(entry.name.clone()))?;
            let project = api.find_project_by_slug(slug)?;
            let files = api.list_files(project.id, &config.pack.minecraft_version, loader)?;
            let file = CandidateSelector::select_latest(&files)?;
            (project, file)
        }
    };

    let mut locked = match DependencyEntry::from_curseforge(&project, &file) {
        Ok(entry) => entry,
        Err(DependencyError::MissingSide { .. }) => {
            DependencyEntry::from_curseforge_with_side(&project, &file, entry.side.clone())?
        }
        Err(err) => return Err(err.into()),
    };
    locked.required = entry.required;
    Ok(ResolvedEntry {
        entry: locked,
        file,
    })
}

fn write_client_zip(
    project_dir: &Path,
    zip_path: &Path,
    config: &Config,
    resolved: &ResolvedLists,
) -> Result<(), BuildError> {
    let file = File::create(zip_path).map_err(|source| BuildError::WriteFile {
        path: zip_path.display().to_string(),
        source,
    })?;
    let mut zip = zip::ZipWriter::new(file);
    let options = SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
    let entries = resolved.client_manifest_entries();
    let manifest = generate_pack_manifest(config, &entries, "overrides")?;
    write_zip_bytes(
        &mut zip,
        "manifest.json",
        &serde_json::to_vec(&manifest)?,
        options,
        zip_path,
    )?;
    write_zip_bytes(
        &mut zip,
        "modlist.html",
        generate_modlist_html(&entries).as_bytes(),
        options,
        zip_path,
    )?;
    add_override_dir(
        &mut zip,
        project_dir,
        "common",
        Some("overrides"),
        options,
        zip_path,
    )?;
    add_override_dir(
        &mut zip,
        project_dir,
        "client",
        Some("overrides"),
        options,
        zip_path,
    )?;
    add_docs(&mut zip, project_dir, options, zip_path)?;
    zip.finish().map_err(|source| BuildError::Zip {
        path: zip_path.display().to_string(),
        source,
    })?;
    Ok(())
}

fn write_server_zip(
    project_dir: &Path,
    zip_path: &Path,
    config: &Config,
    resolved: &ResolvedLists,
    server_api_key: &str,
) -> Result<(), BuildError> {
    let file = File::create(zip_path).map_err(|source| BuildError::WriteFile {
        path: zip_path.display().to_string(),
        source,
    })?;
    let mut zip = zip::ZipWriter::new(file);
    let options = SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
    let manifest = server_files_manifest(config, resolved, server_api_key)?;
    let manifest_json = serde_json::to_vec(&manifest)?;
    write_zip_bytes(
        &mut zip,
        "server-files.json",
        &manifest_json,
        options,
        zip_path,
    )?;
    write_zip_bytes(
        &mut zip,
        "start.bat",
        render_start_bat(&manifest).as_bytes(),
        options,
        zip_path,
    )?;
    write_zip_bytes(
        &mut zip,
        "start.sh",
        render_start_sh(&manifest).as_bytes(),
        options,
        zip_path,
    )?;
    write_zip_bytes(
        &mut zip,
        "mcmpu-download.ps1",
        render_download_ps1().as_bytes(),
        options,
        zip_path,
    )?;
    add_override_dir(&mut zip, project_dir, "common", None, options, zip_path)?;
    add_override_dir(&mut zip, project_dir, "server", None, options, zip_path)?;
    write_zip_bytes(
        &mut zip,
        "resourcepacks/.mcmpu-keep",
        b"",
        options,
        zip_path,
    )?;
    add_docs(&mut zip, project_dir, options, zip_path)?;
    zip.finish().map_err(|source| BuildError::Zip {
        path: zip_path.display().to_string(),
        source,
    })?;
    Ok(())
}

fn server_files_manifest(
    config: &Config,
    resolved: &ResolvedLists,
    server_api_key: &str,
) -> Result<ServerFilesManifest, BuildError> {
    let files = resolved
        .server_downloads()
        .into_iter()
        .map(server_file)
        .collect::<Result<Vec<_>, _>>()?;
    let loader = plan_loader_install(
        &config.pack.loader,
        &config.pack.minecraft_version,
        &config.pack.loader_version,
    )?;
    Ok(ServerFilesManifest {
        language: config.language.clone(),
        curseforge_api_key: server_api_key.to_string(),
        java: ServerJavaConfig {
            min_memory: config.java.server.min_memory.clone(),
            max_memory: config.java.server.max_memory.clone(),
            extra_args: config.java.server.extra_args.clone(),
        },
        files,
        loader,
    })
}

fn server_file(resolved: &ResolvedEntry) -> Result<ServerFile, BuildError> {
    let project_id = resolved.entry.project_id.unwrap_or_default();
    let file_id = resolved.entry.file_id.unwrap_or_default();
    let slug = resolved
        .entry
        .slug
        .as_deref()
        .ok_or_else(|| ResolveError::MissingSlug(resolved.entry.name.clone()))?;
    let file_name = curse_maven_file_name(slug, &resolved.file);
    let path = match resolved.entry.dependency_type {
        DependencyType::Mod => format!("mods/{file_name}"),
        DependencyType::ResourcePack => format!("resourcepacks/{file_name}"),
        DependencyType::ShaderPack => format!("shaderpacks/{file_name}"),
    };
    let sha1 = resolved
        .file
        .hashes
        .iter()
        .find(|hash| hash.algo == 1)
        .map(|hash| hash.value.clone())
        .ok_or(BuildError::MissingSha1 {
            project_id,
            file_id,
        })?;
    let url = resolved
        .file
        .download_url
        .clone()
        .ok_or(BuildError::MissingDownloadUrl {
            project_id,
            file_id,
        })?;
    Ok(ServerFile {
        path,
        url,
        sha1,
        size: resolved.file.file_length,
    })
}

fn curse_maven_file_name(slug: &str, file: &CfFile) -> String {
    let extension = extension_of(&file.file_name);
    format!(
        "{}{slug}-{project_id}-{file_id}.{extension}",
        sort_prefix(&file.file_name),
        project_id = file.mod_id,
        file_id = file.id,
    )
}

fn extension_of(file_name: &str) -> String {
    Path::new(file_name)
        .extension()
        .and_then(|extension| extension.to_str())
        .unwrap_or("jar")
        .to_ascii_lowercase()
}

fn sort_prefix(file_name: &str) -> String {
    file_name
        .chars()
        .take_while(|ch| !ch.is_ascii_alphanumeric())
        .collect()
}

fn add_override_dir(
    zip: &mut zip::ZipWriter<File>,
    project_dir: &Path,
    side: &str,
    zip_prefix: Option<&str>,
    options: SimpleFileOptions,
    zip_path: &Path,
) -> Result<(), BuildError> {
    let dir = project_dir.join("overrides").join(side);
    if !dir.exists() {
        return Ok(());
    }
    for entry in WalkDir::new(&dir) {
        let entry = entry.map_err(|source| BuildError::WalkDir {
            path: dir.display().to_string(),
            source,
        })?;
        if entry.file_type().is_dir() {
            continue;
        }
        let relative = entry
            .path()
            .strip_prefix(&dir)
            .expect("walked path under root");
        let relative = slash_path(relative);
        let zip_name = match zip_prefix {
            Some(prefix) => format!("{prefix}/{relative}"),
            None => relative,
        };
        add_file(zip, entry.path(), &zip_name, options, zip_path)?;
    }
    Ok(())
}

fn add_docs(
    zip: &mut zip::ZipWriter<File>,
    project_dir: &Path,
    options: SimpleFileOptions,
    zip_path: &Path,
) -> Result<(), BuildError> {
    for entry in fs::read_dir(project_dir).map_err(|source| BuildError::ReadFile {
        path: project_dir.display().to_string(),
        source,
    })? {
        let entry = entry.map_err(|source| BuildError::ReadFile {
            path: project_dir.display().to_string(),
            source,
        })?;
        if !entry
            .file_type()
            .map(|file_type| file_type.is_file())
            .unwrap_or(false)
        {
            continue;
        }
        let name = entry.file_name().to_string_lossy().to_string();
        let upper = name.to_ascii_uppercase();
        if ["README", "LICENSE", "NOTICE", "CHANGELOG"]
            .iter()
            .any(|prefix| upper.starts_with(prefix))
        {
            add_file(zip, &entry.path(), &name, options, zip_path)?;
        }
    }
    Ok(())
}

fn add_file(
    zip: &mut zip::ZipWriter<File>,
    source: &Path,
    zip_name: &str,
    options: SimpleFileOptions,
    zip_path: &Path,
) -> Result<(), BuildError> {
    if should_optimize_in_memory(source) {
        let bytes = optimized_file_bytes(source)?;
        return write_zip_bytes(zip, zip_name, &bytes, options, zip_path);
    }

    zip.start_file(zip_name, options)
        .map_err(|source_err| BuildError::Zip {
            path: zip_path.display().to_string(),
            source: source_err,
        })?;
    let mut source_file = File::open(source).map_err(|source_err| BuildError::ReadFile {
        path: source.display().to_string(),
        source: source_err,
    })?;
    std::io::copy(&mut source_file, zip).map_err(|source_err| BuildError::WriteFile {
        path: zip_path.display().to_string(),
        source: source_err,
    })?;
    Ok(())
}

fn optimized_file_bytes(source: &Path) -> Result<Vec<u8>, BuildError> {
    let mut bytes = Vec::new();
    File::open(source)
        .map_err(|source_err| BuildError::ReadFile {
            path: source.display().to_string(),
            source: source_err,
        })?
        .read_to_end(&mut bytes)
        .map_err(|source_err| BuildError::ReadFile {
            path: source.display().to_string(),
            source: source_err,
        })?;
    match normalized_extension(source).as_deref() {
        Some("json") => match minify_json_bytes(&bytes) {
            Ok(optimized) => Ok(optimized),
            Err(source_err) => {
                eprintln!(
                    "[build] warning: skipped JSON minify for '{}': {source_err}",
                    source.display()
                );
                Ok(bytes)
            }
        },
        Some("png") => optimize_png_bytes(&bytes).map_err(|source_err| BuildError::OptimizePng {
            path: source.display().to_string(),
            source: source_err,
        }),
        _ => Ok(bytes),
    }
}

fn should_optimize_in_memory(source: &Path) -> bool {
    matches!(
        normalized_extension(source).as_deref(),
        Some("json" | "png")
    )
}

fn normalized_extension(source: &Path) -> Option<String> {
    source
        .extension()
        .and_then(|extension| extension.to_str())
        .map(|extension| extension.to_ascii_lowercase())
}

fn write_zip_bytes(
    zip: &mut zip::ZipWriter<File>,
    name: &str,
    bytes: &[u8],
    options: SimpleFileOptions,
    zip_path: &Path,
) -> Result<(), BuildError> {
    zip.start_file(name, options)
        .map_err(|source| BuildError::Zip {
            path: zip_path.display().to_string(),
            source,
        })?;
    zip.write_all(bytes)
        .map_err(|source| BuildError::WriteFile {
            path: zip_path.display().to_string(),
            source,
        })
}

fn read_dependency_list(
    path: impl AsRef<Path>,
    side: DependencySide,
) -> Result<Vec<DependencyEntry>, BuildError> {
    let path = path.as_ref();
    let text = fs::read_to_string(path).map_err(|source| BuildError::ReadDependencyList {
        path: path.display().to_string(),
        source,
    })?;
    let mut entries: Vec<DependencyEntry> =
        serde_json::from_str(&text).map_err(|source| BuildError::ParseDependencyList {
            path: path.display().to_string(),
            source,
        })?;
    for entry in &mut entries {
        entry.side = side.clone();
    }
    Ok(entries)
}

fn create_dir(path: impl AsRef<Path>) -> Result<(), BuildError> {
    let path = path.as_ref();
    fs::create_dir_all(path).map_err(|source| BuildError::CreateDir {
        path: path.display().to_string(),
        source,
    })
}

fn slash_path(path: &Path) -> String {
    path.components()
        .map(|component| component.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}
