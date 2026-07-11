use std::collections::{BTreeSet, HashMap};
use std::fs;
use std::hash::Hash;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use thiserror::Error;

use crate::config::{Config, ConfigError};
use crate::curseforge::client::CurseForgeClient;
use crate::curseforge::model::{CfFile, CfProject};
use crate::curseforge::resolver::{ApiKeyResolver, CurseForgeApi, ResolveError};
use crate::dependency::{DependencyEntry, DependencySide, DependencyType};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GuiDependencyList {
    pub entries: Vec<GuiModEntry>,
}

impl GuiDependencyList {
    pub fn load(project_dir: impl AsRef<Path>) -> Result<Self, ModListIoError> {
        let project_dir = project_dir.as_ref();
        let mut entries = Vec::new();
        for manifest in ManifestList::ALL {
            entries.extend(read_manifest(project_dir, manifest)?);
        }
        Ok(Self { entries })
    }

    pub fn save(&self, project_dir: impl AsRef<Path>) -> Result<(), ModListIoError> {
        self.validate()?;
        let project_dir = project_dir.as_ref();
        for manifest in ManifestList::ALL {
            let entries = self
                .entries
                .iter()
                .filter(|entry| entry.manifest == manifest)
                .map(GuiModEntry::to_dependency_entry)
                .collect::<Vec<_>>();
            write_manifest(project_dir, manifest, &entries)?;
        }
        Ok(())
    }

    pub fn validate(&self) -> Result<(), ModListValidationError> {
        for (index, entry) in self.entries.iter().enumerate() {
            entry.validate_at(index)?;
        }
        Ok(())
    }

    pub fn mods(&self) -> Vec<&GuiModEntry> {
        self.entries
            .iter()
            .filter(|entry| entry.dependency_type == GuiDependencyType::Mod)
            .collect()
    }

    pub fn resource_packs(&self) -> Vec<&GuiModEntry> {
        self.entries
            .iter()
            .filter(|entry| entry.dependency_type == GuiDependencyType::ResourcePack)
            .collect()
    }

    pub fn shader_packs(&self) -> Vec<&GuiModEntry> {
        self.entries
            .iter()
            .filter(|entry| entry.dependency_type == GuiDependencyType::ShaderPack)
            .collect()
    }

    pub fn filtered_by_search(&self, search: &str) -> Self {
        let needle = search.trim().to_ascii_lowercase();
        if needle.is_empty() {
            return self.clone();
        }
        Self {
            entries: self
                .entries
                .iter()
                .filter(|entry| entry.matches_search(&needle))
                .cloned()
                .collect(),
        }
    }

    pub fn group_indices(&self, dependency_type: GuiDependencyType, search: &str) -> Vec<usize> {
        let needle = search.trim().to_ascii_lowercase();
        self.entries
            .iter()
            .enumerate()
            .filter_map(|(index, entry)| {
                (entry.dependency_type == dependency_type
                    && (needle.is_empty() || entry.matches_search(&needle)))
                .then_some(index)
            })
            .collect()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GuiModEntry {
    pub manifest: ManifestList,
    pub name: String,
    pub dependency_type: GuiDependencyType,
    pub loader: Option<String>,
    pub project_id: Option<u64>,
    pub file_id: Option<u64>,
    pub auto_update: bool,
    pub slug: Option<String>,
    pub required: bool,
}

impl GuiModEntry {
    pub fn auto_update(&self) -> bool {
        self.auto_update
    }

    pub fn allows_file_version_selection(&self) -> bool {
        !self.auto_update
    }

    pub fn set_auto_update(&mut self, enabled: bool) {
        self.auto_update = enabled;
        if enabled {
            self.file_id = None;
        }
    }

    pub fn validate(&self) -> Result<(), ModListValidationError> {
        self.validate_at(0)
    }

    pub fn validate_at(&self, index: usize) -> Result<(), ModListValidationError> {
        if self.name.trim().is_empty() {
            return Err(ModListValidationError::MissingName { index });
        }
        if self.project_id.is_none() && trimmed_option(self.slug.as_deref()).is_none() {
            return Err(ModListValidationError::MissingProjectLocator { index });
        }
        if self.file_id.is_none() && !self.auto_update {
            return Err(ModListValidationError::MissingFileId { index });
        }
        if let Some(loader) = self.loader.as_deref() {
            validate_loader(loader, index)?;
        }
        match self.manifest {
            ManifestList::Common | ManifestList::Client | ManifestList::Server => {
                if self.dependency_type != GuiDependencyType::Mod {
                    return Err(ModListValidationError::InvalidModManifestType { index });
                }
            }
            ManifestList::Resourcepacks => {
                if self.dependency_type == GuiDependencyType::Mod {
                    return Err(ModListValidationError::InvalidResourcepackManifestType { index });
                }
            }
        }
        Ok(())
    }

    pub fn loader_options(
        &self,
        config_loader: Option<&str>,
        candidates: &[CfFile],
    ) -> Vec<Option<String>> {
        let mut loaders = BTreeSet::new();
        for loader in [self.loader.as_deref(), config_loader]
            .into_iter()
            .flatten()
        {
            if let Some(loader) = normalized_loader(loader) {
                loaders.insert(loader);
            }
        }
        for file in candidates {
            if let Some(loader) = file.loader().and_then(|loader| normalized_loader(&loader)) {
                loaders.insert(loader);
            }
        }
        std::iter::once(None)
            .chain(loaders.into_iter().map(Some))
            .collect()
    }

    fn from_dependency_entry(manifest: ManifestList, entry: DependencyEntry) -> Self {
        Self {
            manifest,
            name: entry.name,
            dependency_type: entry.dependency_type.into(),
            loader: trimmed_option(entry.loader.as_deref()).map(str::to_string),
            project_id: entry.project_id,
            file_id: entry.file_id,
            auto_update: entry.file_id.is_none(),
            slug: trimmed_option(entry.slug.as_deref()).map(str::to_string),
            required: entry.required,
        }
    }

    fn to_dependency_entry(&self) -> DependencyEntry {
        DependencyEntry {
            project_id: self.project_id,
            file_id: if self.auto_update { None } else { self.file_id },
            slug: trimmed_option(self.slug.as_deref()).map(str::to_string),
            loader: self.loader.as_deref().and_then(normalized_loader),
            side: self.manifest.side(),
            dependency_type: self.dependency_type.into(),
            name: self.name.trim().to_string(),
            authors: Vec::new(),
            required: self.required,
        }
    }

    fn matches_search(&self, needle: &str) -> bool {
        contains_ignore_ascii_case(&self.name, needle)
            || self
                .slug
                .as_deref()
                .is_some_and(|slug| contains_ignore_ascii_case(slug, needle))
            || self
                .project_id
                .is_some_and(|project_id| project_id.to_string().contains(needle))
            || self
                .file_id
                .is_some_and(|file_id| file_id.to_string().contains(needle))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ManifestList {
    Common,
    Client,
    Server,
    Resourcepacks,
}

impl ManifestList {
    pub const ALL: [Self; 4] = [
        Self::Common,
        Self::Client,
        Self::Server,
        Self::Resourcepacks,
    ];

    pub fn file_name(self) -> &'static str {
        match self {
            Self::Common => "common.json",
            Self::Client => "client.json",
            Self::Server => "server.json",
            Self::Resourcepacks => "resourcepacks.json",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Common => "common",
            Self::Client => "client",
            Self::Server => "server",
            Self::Resourcepacks => "resourcepacks",
        }
    }

    fn side(self) -> DependencySide {
        match self {
            Self::Common => DependencySide::Common,
            Self::Client => DependencySide::Client,
            Self::Server => DependencySide::Server,
            Self::Resourcepacks => DependencySide::Resourcepack,
        }
    }
}

pub type ModListManifest = ManifestList;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum GuiDependencyType {
    Mod,
    ResourcePack,
    ShaderPack,
}

impl GuiDependencyType {
    pub const ALL: [Self; 3] = [Self::Mod, Self::ResourcePack, Self::ShaderPack];

    pub fn label(self) -> &'static str {
        match self {
            Self::Mod => "mod",
            Self::ResourcePack => "resource_pack",
            Self::ShaderPack => "shader_pack",
        }
    }
}

impl From<DependencyType> for GuiDependencyType {
    fn from(value: DependencyType) -> Self {
        match value {
            DependencyType::Mod => Self::Mod,
            DependencyType::ResourcePack => Self::ResourcePack,
            DependencyType::ShaderPack => Self::ShaderPack,
        }
    }
}

impl From<GuiDependencyType> for DependencyType {
    fn from(value: GuiDependencyType) -> Self {
        match value {
            GuiDependencyType::Mod => Self::Mod,
            GuiDependencyType::ResourcePack => Self::ResourcePack,
            GuiDependencyType::ShaderPack => Self::ShaderPack,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ProjectLookup {
    ProjectId(u64),
    Slug(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ProjectSearchInput {
    ProjectId(u64),
    Keywords(String),
}

pub fn parse_project_search_input(
    input: &str,
) -> Result<Option<ProjectSearchInput>, ModListInputError> {
    let raw = input.trim();
    if raw.is_empty() {
        return Ok(None);
    }
    if let Some(lookup) = parse_curseforge_url(raw)? {
        return Ok(Some(match lookup {
            ProjectLookup::ProjectId(project_id) => ProjectSearchInput::ProjectId(project_id),
            ProjectLookup::Slug(slug) => ProjectSearchInput::Keywords(slug),
        }));
    }
    if raw.chars().all(|ch| ch.is_ascii_digit()) {
        return Ok(Some(ProjectSearchInput::ProjectId(parse_project_id(raw)?)));
    }
    Ok(Some(ProjectSearchInput::Keywords(raw.to_string())))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueryFileCandidates {
    pub project: ProjectLookup,
    pub minecraft_version: String,
    pub loader: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileCandidate {
    pub file_id: u64,
    pub file_name: String,
    pub display_name: Option<String>,
    pub game_versions: Vec<String>,
    pub loader: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectCandidate {
    pub project_id: u64,
    pub slug: String,
    pub name: String,
    pub authors: Vec<String>,
    pub dependency_type: GuiDependencyType,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AddProjectDraft {
    pub project: ProjectCandidate,
    pub manifest: ManifestList,
    pub loader: Option<String>,
    pub file_id: Option<u64>,
    pub auto_update: bool,
    pub required: bool,
}

impl AddProjectDraft {
    pub fn from_candidate(project: ProjectCandidate, config_loader: Option<&str>) -> Self {
        let manifest = match project.dependency_type {
            GuiDependencyType::Mod => ManifestList::Common,
            GuiDependencyType::ResourcePack | GuiDependencyType::ShaderPack => {
                ManifestList::Resourcepacks
            }
        };
        Self {
            project,
            manifest,
            loader: config_loader.and_then(normalized_loader),
            file_id: None,
            auto_update: true,
            required: true,
        }
    }

    pub fn into_entry(self) -> GuiModEntry {
        let mut entry = entry_from_project_candidate(&self.project, self.manifest, self.loader);
        entry.file_id = self.file_id;
        entry.auto_update = self.auto_update;
        if entry.auto_update {
            entry.file_id = None;
        }
        entry.required = self.required;
        entry
    }

    pub fn allows_file_version_selection(&self) -> bool {
        !self.auto_update
    }

    pub fn select_file_version(&mut self, file_id: u64) {
        self.auto_update = false;
        self.file_id = Some(file_id);
    }
}

pub fn search_project_candidates<A: CurseForgeApi>(
    api: &A,
    input: &ProjectSearchInput,
) -> Result<Vec<ProjectCandidate>, ModListApiError> {
    let projects = match input {
        ProjectSearchInput::ProjectId(project_id) => vec![api.get_project(*project_id)?],
        ProjectSearchInput::Keywords(query) => api.search_projects(query)?,
    };
    Ok(projects
        .into_iter()
        .filter_map(|project| project_candidate(project).ok())
        .collect())
}

pub fn query_file_candidates<A: CurseForgeApi>(
    api: &A,
    query: &QueryFileCandidates,
) -> Result<Vec<FileCandidate>, ModListApiError> {
    let project = find_project(api, &query.project)?;
    let loader = query.loader.as_deref().and_then(normalized_loader);
    let files = api.list_files(
        project.id,
        query.minecraft_version.trim(),
        loader.as_deref().unwrap_or(""),
    )?;
    Ok(files.into_iter().map(file_candidate).collect())
}

#[derive(Debug, Default)]
pub struct QueryFileCandidateCache {
    candidates: Mutex<HashMap<QueryFileCandidateCacheKey, CacheCell<Vec<FileCandidate>>>>,
}

impl QueryFileCandidateCache {
    pub fn clear(&self) {
        self.candidates
            .lock()
            .expect("file candidate cache mutex poisoned")
            .clear();
    }
}

pub fn query_file_candidates_cached<A: CurseForgeApi>(
    api: &A,
    cache: &QueryFileCandidateCache,
    query: &QueryFileCandidates,
) -> Result<Vec<FileCandidate>, ModListApiError> {
    cached_lookup(
        &cache.candidates,
        QueryFileCandidateCacheKey::from(query),
        || query_file_candidates(api, query),
    )
}

pub fn entry_from_project_candidate(
    project: &ProjectCandidate,
    manifest: ManifestList,
    loader: Option<String>,
) -> GuiModEntry {
    GuiModEntry {
        manifest,
        name: project.name.clone(),
        dependency_type: project.dependency_type,
        loader: loader.and_then(|loader| normalized_loader(&loader)),
        project_id: Some(project.project_id),
        file_id: None,
        auto_update: true,
        slug: Some(project.slug.clone()),
        required: true,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModListProjectConfig {
    pub minecraft_version: String,
    pub loader: String,
    pub api_key: String,
    pub proxy: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModListProjectSettings {
    pub minecraft_version: String,
    pub loader: String,
}

pub fn load_project_settings_for_mod_list(
    project_dir: &Path,
) -> Result<ModListProjectSettings, ConfigError> {
    let config = Config::from_path(project_dir.join("config.toml"))?;
    Ok(ModListProjectSettings {
        minecraft_version: config.pack.minecraft_version,
        loader: config.pack.loader.manifest_name().to_string(),
    })
}

pub fn load_project_config_for_mod_list(
    project_dir: &Path,
) -> Result<ModListProjectConfig, ModListConfigError> {
    let config = Config::from_path(project_dir.join("config.toml"))?;
    let env_key = ApiKeyResolver::env_key();
    let builtin_key = ApiKeyResolver::builtin_key();
    let api_key = ApiKeyResolver::resolve(
        None,
        env_key.as_deref(),
        builtin_key.as_deref().unwrap_or(""),
    )?;
    Ok(ModListProjectConfig {
        minecraft_version: config.pack.minecraft_version,
        loader: config.pack.loader.manifest_name().to_string(),
        api_key,
        proxy: config.network.proxy_url()?,
    })
}

type CacheCell<T> = Arc<OnceLock<Result<T, ModListApiError>>>;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct QueryFileCandidateCacheKey {
    project: ProjectLookup,
    minecraft_version: String,
    loader: Option<String>,
}

impl From<&QueryFileCandidates> for QueryFileCandidateCacheKey {
    fn from(query: &QueryFileCandidates) -> Self {
        Self {
            project: query.project.clone(),
            minecraft_version: query.minecraft_version.trim().to_string(),
            loader: query.loader.as_deref().and_then(normalized_loader),
        }
    }
}

pub fn build_curseforge_client(
    config: &ModListProjectConfig,
) -> Result<CurseForgeClient, ResolveError> {
    CurseForgeClient::with_proxy(config.api_key.clone(), config.proxy.as_deref())
}

#[derive(Debug, Error)]
pub enum ModListIoError {
    #[error("failed to read {path}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to parse {path}: {source}")]
    Parse {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error("failed to serialize {path}: {source}")]
    Serialize {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error("failed to write {path}: {source}")]
    Write {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("{0}")]
    Validation(#[from] ModListValidationError),
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum ModListValidationError {
    #[error("entry {index} is missing name")]
    MissingName { index: usize },
    #[error("entry {index} must have slug or project_id")]
    MissingProjectLocator { index: usize },
    #[error("entry {index} requires file_id when auto update is disabled")]
    MissingFileId { index: usize },
    #[error("entry {index} has unsupported loader '{loader}'")]
    InvalidLoader { index: usize, loader: String },
    #[error("entry {index} in common/client/server must have type mod")]
    InvalidModManifestType { index: usize },
    #[error("entry {index} in resourcepacks must have type resource_pack or shader_pack")]
    InvalidResourcepackManifestType { index: usize },
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum ModListInputError {
    #[error("project input is required")]
    Empty,
    #[error("invalid CurseForge URL '{value}'")]
    InvalidUrl { value: String },
    #[error("invalid project id '{value}'")]
    InvalidProjectId { value: String },
    #[error("invalid slug '{value}'")]
    InvalidSlug { value: String },
}

#[derive(Debug, Error)]
pub enum ModListApiError {
    #[error("unsupported CurseForge class id {0}")]
    UnsupportedClass(u64),
    #[error("CurseForge API error: {0}")]
    Api(#[from] ResolveError),
}

impl Clone for ModListApiError {
    fn clone(&self) -> Self {
        match self {
            Self::UnsupportedClass(class_id) => Self::UnsupportedClass(*class_id),
            Self::Api(err) => Self::Api(err.clone()),
        }
    }
}

impl PartialEq for ModListApiError {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::UnsupportedClass(left), Self::UnsupportedClass(right)) => left == right,
            (Self::Api(left), Self::Api(right)) => left.to_string() == right.to_string(),
            _ => false,
        }
    }
}

impl Eq for ModListApiError {}

#[derive(Debug, Error)]
pub enum ModListConfigError {
    #[error("{0}")]
    Config(#[from] ConfigError),
    #[error("{0}")]
    ApiKey(#[from] ResolveError),
}

fn read_manifest(
    project_dir: &Path,
    manifest: ManifestList,
) -> Result<Vec<GuiModEntry>, ModListIoError> {
    let path = project_dir.join(manifest.file_name());
    let text = fs::read_to_string(&path).map_err(|source| ModListIoError::Read {
        path: path.clone(),
        source,
    })?;
    let entries = serde_json::from_str::<Vec<DependencyEntry>>(&text).map_err(|source| {
        ModListIoError::Parse {
            path: path.clone(),
            source,
        }
    })?;
    Ok(entries
        .into_iter()
        .map(|entry| GuiModEntry::from_dependency_entry(manifest, entry))
        .collect())
}

fn write_manifest(
    project_dir: &Path,
    manifest: ManifestList,
    entries: &[DependencyEntry],
) -> Result<(), ModListIoError> {
    let path = project_dir.join(manifest.file_name());
    let text =
        serde_json::to_string_pretty(entries).map_err(|source| ModListIoError::Serialize {
            path: path.clone(),
            source,
        })?;
    fs::write(&path, text).map_err(|source| ModListIoError::Write { path, source })
}

fn find_project<A: CurseForgeApi>(
    api: &A,
    lookup: &ProjectLookup,
) -> Result<CfProject, ResolveError> {
    match lookup {
        ProjectLookup::ProjectId(project_id) => api.get_project(*project_id),
        ProjectLookup::Slug(slug) => api.find_project_by_slug(slug),
    }
}

fn project_candidate(project: CfProject) -> Result<ProjectCandidate, ModListApiError> {
    let dependency_type = match project.class_id {
        6 => GuiDependencyType::Mod,
        12 => GuiDependencyType::ResourcePack,
        6552 => GuiDependencyType::ShaderPack,
        class_id => return Err(ModListApiError::UnsupportedClass(class_id)),
    };
    let authors = project.author_names();
    Ok(ProjectCandidate {
        project_id: project.id,
        slug: project.slug,
        name: project.name,
        authors,
        dependency_type,
    })
}

fn file_candidate(file: CfFile) -> FileCandidate {
    let loader = file.loader();
    FileCandidate {
        file_id: file.id,
        file_name: file.file_name,
        display_name: file.display_name,
        game_versions: file.game_versions.iter().cloned().collect(),
        loader,
    }
}

fn parse_curseforge_url(raw: &str) -> Result<Option<ProjectLookup>, ModListInputError> {
    let Some(after_scheme) = raw
        .strip_prefix("https://")
        .or_else(|| raw.strip_prefix("http://"))
    else {
        return Ok(None);
    };
    let Some(path_start) = after_scheme.find('/') else {
        return Err(ModListInputError::InvalidUrl {
            value: raw.to_string(),
        });
    };
    let host = &after_scheme[..path_start];
    if !host.eq_ignore_ascii_case("www.curseforge.com")
        && !host.eq_ignore_ascii_case("curseforge.com")
    {
        return Err(ModListInputError::InvalidUrl {
            value: raw.to_string(),
        });
    }
    let path = after_scheme[path_start + 1..]
        .split(['?', '#'])
        .next()
        .unwrap_or_default()
        .trim_matches('/');
    let parts = path.split('/').collect::<Vec<_>>();
    if parts.len() == 2 && parts[0].eq_ignore_ascii_case("projects") {
        return Ok(Some(ProjectLookup::ProjectId(parse_project_id(parts[1])?)));
    }
    if parts.len() >= 3
        && parts[0].eq_ignore_ascii_case("minecraft")
        && let Some(slug) = parts.last().and_then(|value| normalize_slug(value))
    {
        return Ok(Some(ProjectLookup::Slug(slug)));
    }
    Err(ModListInputError::InvalidUrl {
        value: raw.to_string(),
    })
}

fn parse_project_id(raw: &str) -> Result<u64, ModListInputError> {
    raw.parse::<u64>()
        .ok()
        .filter(|value| *value > 0)
        .ok_or_else(|| ModListInputError::InvalidProjectId {
            value: raw.to_string(),
        })
}

fn normalize_slug(raw: &str) -> Option<String> {
    let slug = raw.trim().trim_matches('/');
    if slug.is_empty()
        || !slug
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '_')
    {
        return None;
    }
    Some(slug.to_ascii_lowercase())
}

fn normalized_loader(raw: &str) -> Option<String> {
    let loader = raw.trim().to_ascii_lowercase();
    if loader.is_empty() {
        None
    } else {
        Some(loader)
    }
}

fn trimmed_option(raw: Option<&str>) -> Option<&str> {
    raw.map(str::trim).filter(|value| !value.is_empty())
}

fn contains_ignore_ascii_case(value: &str, needle: &str) -> bool {
    value.to_ascii_lowercase().contains(needle)
}

fn cached_lookup<K, V, F>(
    map: &Mutex<HashMap<K, CacheCell<V>>>,
    key: K,
    fetch: F,
) -> Result<V, ModListApiError>
where
    K: Eq + Hash,
    V: Clone,
    F: FnOnce() -> Result<V, ModListApiError>,
{
    let cell = {
        let mut map = map.lock().expect("file candidate cache mutex poisoned");
        map.entry(key)
            .or_insert_with(|| Arc::new(OnceLock::new()))
            .clone()
    };
    cell.get_or_init(fetch).clone()
}

fn validate_loader(loader: &str, index: usize) -> Result<(), ModListValidationError> {
    const VALID_LOADERS: &[&str] = &[
        "forge",
        "fabric",
        "neoforge",
        "cleanroom",
        "quilt",
        "liteloader",
        "cauldron",
    ];
    let Some(loader) = normalized_loader(loader) else {
        return Ok(());
    };
    if VALID_LOADERS.contains(&loader.as_str()) {
        Ok(())
    } else {
        Err(ModListValidationError::InvalidLoader { index, loader })
    }
}
