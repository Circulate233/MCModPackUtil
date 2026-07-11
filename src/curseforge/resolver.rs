use std::collections::HashMap;
use std::hash::Hash;
use std::sync::{Arc, Mutex, OnceLock};

use thiserror::Error;

use crate::curseforge::model::{CfFile, CfProject, ReleaseType};
use crate::dependency::DependencyEntry;

#[derive(Debug, Error, Clone)]
pub enum ResolveError {
    #[error(
        "CurseForge API key is required. Create one at https://console.curseforge.com/ and configure it with --curseforge-api-key, runtime environment variable MCMODPACKUTIL_CURSEFORGE_API_KEY, CURSEFORGE_API_KEY, CF_API_KEY, or an official build with an embedded key"
    )]
    MissingApiKey,
    #[error("dependency '{0}' is missing slug and numeric project_id/file_id")]
    MissingSlug(String),
    #[error("no CurseForge file candidates matched the requested Minecraft version and loader")]
    NoCandidates,
    #[error(
        "only alpha CurseForge file candidates were found; specify file_id explicitly to use alpha files"
    )]
    AlphaRequiresExplicitFileId,
    #[error("CurseForge file {project_id}/{file_id} has no download URL")]
    MissingDownloadUrl { project_id: u64, file_id: u64 },
    #[error("CurseForge API error: {0}")]
    Api(String),
}

pub struct ApiKeyResolver;

impl ApiKeyResolver {
    pub const ENV_KEYS: &'static [&'static str] = &[
        "MCMODPACKUTIL_CURSEFORGE_API_KEY",
        "CURSEFORGE_API_KEY",
        "CF_API_KEY",
    ];

    pub fn env_key() -> Option<String> {
        Self::ENV_KEYS
            .iter()
            .find_map(|name| read_env_var(name))
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
    }

    pub fn builtin_key() -> Option<String> {
        option_env!("MCMPU_BUILTIN_CURSEFORGE_API_KEY")
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(ToOwned::to_owned)
    }

    pub fn resolve(
        cli_key: Option<&str>,
        env_key: Option<&str>,
        builtin_key: &str,
    ) -> Result<String, ResolveError> {
        for candidate in [cli_key, env_key, Some(builtin_key)].into_iter().flatten() {
            let trimmed = candidate.trim();
            if !trimmed.is_empty() {
                return Ok(trimmed.to_string());
            }
        }
        Err(ResolveError::MissingApiKey)
    }
}

fn read_env_var(name: &str) -> Option<String> {
    match std::env::var(name) {
        Ok(value) => {
            let trimmed = value.trim();
            return (!trimmed.is_empty()).then(|| trimmed.to_string());
        }
        Err(std::env::VarError::NotUnicode(_)) => return None,
        Err(std::env::VarError::NotPresent) => {}
    }
    #[cfg(windows)]
    {
        if std::env::var_os("MCMPU_DISABLE_PERSISTENT_ENV").is_some() {
            return None;
        }
        if let Some(value) = read_windows_environment_variable(name, "User") {
            return Some(value);
        }
        if let Some(value) = read_windows_environment_variable(name, "Machine") {
            return Some(value);
        }
    }
    None
}

#[cfg(windows)]
fn read_windows_environment_variable(name: &str, scope: &str) -> Option<String> {
    let output = std::process::Command::new("powershell")
        .args([
            "-NoProfile",
            "-Command",
            &format!("[Environment]::GetEnvironmentVariable('{name}', '{scope}')"),
        ])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let value = String::from_utf8(output.stdout).ok()?;
    let trimmed = value.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

pub struct CandidateSelector;

impl CandidateSelector {
    pub fn select_latest(files: &[CfFile]) -> Result<CfFile, ResolveError> {
        if files.is_empty() {
            return Err(ResolveError::NoCandidates);
        }

        if let Some(file) = files
            .iter()
            .find(|file| file.release_type == ReleaseType::Stable)
        {
            return Ok(file.clone());
        }
        if let Some(file) = files
            .iter()
            .find(|file| file.release_type == ReleaseType::Beta)
        {
            return Ok(file.clone());
        }

        Err(ResolveError::AlphaRequiresExplicitFileId)
    }
}

pub trait CurseForgeApi {
    fn get_project(&self, project_id: u64) -> Result<CfProject, ResolveError>;
    fn get_file(&self, project_id: u64, file_id: u64) -> Result<CfFile, ResolveError>;
    fn find_project_by_slug(&self, slug: &str) -> Result<CfProject, ResolveError>;
    fn search_projects(&self, query: &str) -> Result<Vec<CfProject>, ResolveError> {
        Err(ResolveError::Api(format!(
            "CurseForge project search is unavailable for query '{query}'"
        )))
    }
    fn list_files(
        &self,
        project_id: u64,
        minecraft_version: &str,
        loader: &str,
    ) -> Result<Vec<CfFile>, ResolveError>;

    fn list_project_files(&self, project_id: u64) -> Result<Vec<CfFile>, ResolveError> {
        self.list_files(project_id, "", "")
    }
}

type CacheCell<T> = Arc<OnceLock<Result<T, ResolveError>>>;
type ProjectCache = Mutex<HashMap<u64, CacheCell<CfProject>>>;
type FileCache = Mutex<HashMap<(u64, u64), CacheCell<CfFile>>>;
type SlugCache = Mutex<HashMap<String, CacheCell<CfProject>>>;
type FileListKey = (u64, String, String);
type FileListCache = Mutex<HashMap<FileListKey, CacheCell<Vec<CfFile>>>>;
type ProjectFileListCache = Mutex<HashMap<u64, CacheCell<Vec<CfFile>>>>;

pub struct CachedCurseForgeApi<'a, A> {
    api: &'a A,
    projects: ProjectCache,
    files: FileCache,
    slugs: SlugCache,
    file_lists: FileListCache,
    project_file_lists: ProjectFileListCache,
}

impl<'a, A> CachedCurseForgeApi<'a, A> {
    pub fn new(api: &'a A) -> Self {
        Self {
            api,
            projects: Mutex::new(HashMap::new()),
            files: Mutex::new(HashMap::new()),
            slugs: Mutex::new(HashMap::new()),
            file_lists: Mutex::new(HashMap::new()),
            project_file_lists: Mutex::new(HashMap::new()),
        }
    }
}

impl<A: CurseForgeApi> CurseForgeApi for CachedCurseForgeApi<'_, A> {
    fn get_project(&self, project_id: u64) -> Result<CfProject, ResolveError> {
        cached_lookup(&self.projects, project_id, || {
            self.api.get_project(project_id)
        })
    }

    fn get_file(&self, project_id: u64, file_id: u64) -> Result<CfFile, ResolveError> {
        cached_lookup(&self.files, (project_id, file_id), || {
            self.api.get_file(project_id, file_id)
        })
    }

    fn find_project_by_slug(&self, slug: &str) -> Result<CfProject, ResolveError> {
        cached_lookup(&self.slugs, slug.to_string(), || {
            self.api.find_project_by_slug(slug)
        })
    }

    fn search_projects(&self, query: &str) -> Result<Vec<CfProject>, ResolveError> {
        self.api.search_projects(query)
    }

    fn list_files(
        &self,
        project_id: u64,
        minecraft_version: &str,
        loader: &str,
    ) -> Result<Vec<CfFile>, ResolveError> {
        cached_lookup(
            &self.file_lists,
            (
                project_id,
                minecraft_version.to_string(),
                loader.to_string(),
            ),
            || self.api.list_files(project_id, minecraft_version, loader),
        )
    }

    fn list_project_files(&self, project_id: u64) -> Result<Vec<CfFile>, ResolveError> {
        cached_lookup(&self.project_file_lists, project_id, || {
            self.api.list_project_files(project_id)
        })
    }
}

fn cached_lookup<K, V, F>(
    map: &Mutex<HashMap<K, CacheCell<V>>>,
    key: K,
    fetch: F,
) -> Result<V, ResolveError>
where
    K: Eq + Hash,
    V: Clone,
    F: FnOnce() -> Result<V, ResolveError>,
{
    let cell = {
        let mut map = map.lock().expect("CurseForge API cache mutex poisoned");
        map.entry(key)
            .or_insert_with(|| Arc::new(OnceLock::new()))
            .clone()
    };
    cell.get_or_init(fetch).clone()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedDependency {
    pub project: CfProject,
    pub file: CfFile,
}

pub struct DependencyResolver<A> {
    api: A,
}

impl<A> DependencyResolver<A> {
    pub fn new(api: A) -> Self {
        Self { api }
    }

    pub fn api(&self) -> &A {
        &self.api
    }
}

impl<A: CurseForgeApi> DependencyResolver<A> {
    pub fn resolve_entry(
        &self,
        entry: &DependencyEntry,
        minecraft_version: &str,
        _default_loader: &str,
    ) -> Result<ResolvedDependency, ResolveError> {
        let (project, file) = match (entry.project_id, entry.file_id) {
            (Some(project_id), Some(file_id)) => {
                let project = self.api.get_project(project_id)?;
                let file = self.api.get_file(project_id, file_id)?;
                (project, file)
            }
            _ => {
                let slug = entry
                    .slug
                    .as_deref()
                    .ok_or_else(|| ResolveError::MissingSlug(entry.name.clone()))?;
                let loader = entry.loader.as_deref().unwrap_or("");
                let project = self.api.find_project_by_slug(slug)?;
                let files = self.api.list_files(project.id, minecraft_version, loader)?;
                let file = CandidateSelector::select_latest(&files)?;
                (project, file)
            }
        };

        if file
            .download_url
            .as_deref()
            .unwrap_or_default()
            .trim()
            .is_empty()
        {
            return Err(ResolveError::MissingDownloadUrl {
                project_id: project.id,
                file_id: file.id,
            });
        }

        Ok(ResolvedDependency { project, file })
    }
}
