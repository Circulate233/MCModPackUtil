use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use thiserror::Error;

use crate::config::LoaderKind;

const DEFAULT_LIBRARY_BASE_URL: &str = "https://libraries.minecraft.net/";
const CLEANROOM_RELEASES_BASE_URL: &str = "https://repo.cleanroommc.com/releases/";

/// Supplies installed or downloaded Minecraft version JSON by version id.
pub trait VersionJsonSource {
    fn load_version_json(&self, version_id: &str) -> Result<String, LaunchResolveError>;
}

/// Reads `<versions_dir>/<id>/<id>.json` from a standard launcher layout.
#[derive(Debug, Clone)]
pub struct FileVersionJsonSource {
    versions_dir: PathBuf,
}

impl FileVersionJsonSource {
    pub fn new(versions_dir: impl Into<PathBuf>) -> Self {
        Self {
            versions_dir: versions_dir.into(),
        }
    }

    pub fn versions_dir(&self) -> &Path {
        &self.versions_dir
    }
}

impl VersionJsonSource for FileVersionJsonSource {
    fn load_version_json(&self, version_id: &str) -> Result<String, LaunchResolveError> {
        let path = self
            .versions_dir
            .join(version_id)
            .join(format!("{version_id}.json"));
        fs::read_to_string(&path).map_err(|source| LaunchResolveError::ReadVersionJson {
            version_id: version_id.to_string(),
            path,
            source,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LaunchSide {
    Client,
    Server,
}

/// Rule inputs used by Mojang version JSON. OS-version expressions are matched by
/// exact expression identity supplied by the caller, which keeps regex evaluation
/// outside this module and makes unsupported runtime matching explicit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchEnvironment {
    pub os_name: String,
    pub os_arch: String,
    pub arch_bits: u16,
    pub features: BTreeMap<String, bool>,
    pub matching_os_version_rules: BTreeSet<String>,
}

impl LaunchEnvironment {
    pub fn new(os_name: impl Into<String>, os_arch: impl Into<String>, arch_bits: u16) -> Self {
        Self {
            os_name: os_name.into(),
            os_arch: os_arch.into(),
            arch_bits,
            features: BTreeMap::new(),
            matching_os_version_rules: BTreeSet::new(),
        }
    }

    pub fn with_feature(mut self, name: impl Into<String>, value: bool) -> Self {
        self.features.insert(name.into(), value);
        self
    }

    pub fn with_matching_os_version_rule(mut self, expression: impl Into<String>) -> Self {
        self.matching_os_version_rules.insert(expression.into());
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchArtifact {
    pub path: String,
    pub url: Option<String>,
    pub sha1: Option<String>,
    pub size: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NativeArtifact {
    pub library_name: String,
    pub classifier: String,
    pub artifact: LaunchArtifact,
    pub extract_excludes: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AssetIndexMetadata {
    pub id: String,
    pub url: String,
    pub sha1: String,
    pub size: u64,
    pub total_size: u64,
}

/// Fully resolved metadata needed to construct a Java launch invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchDescriptor {
    pub loader: LoaderKind,
    pub side: LaunchSide,
    pub version_id: String,
    pub inheritance_chain: Vec<String>,
    pub main_class: String,
    pub libraries: Vec<LaunchArtifact>,
    pub classpath: Vec<String>,
    pub client_jar: Option<LaunchArtifact>,
    pub jvm_arguments: Vec<String>,
    pub game_arguments: Vec<String>,
    pub assets: Option<String>,
    pub asset_index: Option<AssetIndexMetadata>,
    pub natives: Vec<NativeArtifact>,
    pub java_version_major: Option<u16>,
}

impl LaunchDescriptor {
    /// Substitutes `${name}` placeholders and fails if any value is unavailable.
    pub fn substitute_arguments(
        &self,
        variables: &BTreeMap<String, String>,
    ) -> Result<(Vec<String>, Vec<String>), LaunchResolveError> {
        Ok((
            substitute_all(&self.jvm_arguments, variables)?,
            substitute_all(&self.game_arguments, variables)?,
        ))
    }
}

/// Explicit metadata emitted by or derived from an installed server distribution.
/// The resolver deliberately does not infer loader-specific server commands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerLaunchMetadata {
    pub version_id: String,
    pub main_class: String,
    pub libraries: Vec<LaunchArtifact>,
    pub jvm_arguments: Vec<String>,
    pub game_arguments: Vec<String>,
    pub java_version_major: Option<u16>,
}

pub struct LoaderLaunchResolver<S> {
    source: S,
}

impl<S> LoaderLaunchResolver<S> {
    pub fn new(source: S) -> Self {
        Self { source }
    }

    pub fn source(&self) -> &S {
        &self.source
    }
}

impl<S: VersionJsonSource> LoaderLaunchResolver<S> {
    pub fn resolve_client(
        &self,
        loader: LoaderKind,
        version_id: &str,
        environment: &LaunchEnvironment,
    ) -> Result<LaunchDescriptor, LaunchResolveError> {
        validate_environment(environment)?;
        let profiles = self.load_inheritance_chain(version_id)?;
        resolve_client_profiles(loader, version_id, profiles, environment)
    }

    pub fn resolve_server(
        &self,
        loader: LoaderKind,
        metadata: ServerLaunchMetadata,
    ) -> Result<LaunchDescriptor, LaunchResolveError> {
        if metadata.version_id.trim().is_empty() {
            return Err(LaunchResolveError::InvalidServerMetadata {
                reason: "version_id must not be empty".to_string(),
            });
        }
        if metadata.main_class.trim().is_empty() {
            return Err(LaunchResolveError::InvalidServerMetadata {
                reason: "main_class must not be empty".to_string(),
            });
        }
        validate_artifacts(&metadata.libraries)?;
        let classpath = unique_paths(
            metadata
                .libraries
                .iter()
                .map(|artifact| artifact.path.clone()),
        );
        Ok(LaunchDescriptor {
            loader,
            side: LaunchSide::Server,
            version_id: metadata.version_id.clone(),
            inheritance_chain: vec![metadata.version_id],
            main_class: metadata.main_class,
            libraries: metadata.libraries,
            classpath,
            client_jar: None,
            jvm_arguments: metadata.jvm_arguments,
            game_arguments: metadata.game_arguments,
            assets: None,
            asset_index: None,
            natives: Vec::new(),
            java_version_major: metadata.java_version_major,
        })
    }

    fn load_inheritance_chain(
        &self,
        requested_id: &str,
    ) -> Result<Vec<RawVersion>, LaunchResolveError> {
        if requested_id.trim().is_empty() {
            return Err(LaunchResolveError::VersionNotFound {
                version_id: requested_id.to_string(),
            });
        }
        let mut visited = BTreeSet::new();
        let mut child_to_parent = Vec::new();
        let mut current = requested_id.to_string();
        loop {
            if !visited.insert(current.clone()) {
                return Err(LaunchResolveError::InheritanceCycle {
                    version_id: current,
                });
            }
            let text = self.source.load_version_json(&current)?;
            let profile: RawVersion = serde_json::from_str(&text).map_err(|source| {
                LaunchResolveError::ParseVersionJson {
                    version_id: current.clone(),
                    source,
                }
            })?;
            if profile.id != current {
                return Err(LaunchResolveError::VersionIdMismatch {
                    requested: current,
                    actual: profile.id,
                });
            }
            current = match profile.inherits_from.clone() {
                Some(parent) if !parent.trim().is_empty() => parent,
                Some(_) => {
                    return Err(LaunchResolveError::EmptyParentVersion {
                        version_id: profile.id,
                    });
                }
                None => {
                    child_to_parent.push(profile);
                    break;
                }
            };
            child_to_parent.push(profile);
        }
        child_to_parent.reverse();
        Ok(child_to_parent)
    }
}

#[derive(Debug, Error)]
pub enum LaunchResolveError {
    #[error("version JSON '{version_id}' was not found")]
    VersionNotFound { version_id: String },
    #[error("failed to read version JSON '{version_id}' from '{path}': {source}")]
    ReadVersionJson {
        version_id: String,
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to parse version JSON '{version_id}': {source}")]
    ParseVersionJson {
        version_id: String,
        #[source]
        source: serde_json::Error,
    },
    #[error("version JSON id mismatch: requested '{requested}', document contains '{actual}'")]
    VersionIdMismatch { requested: String, actual: String },
    #[error("version '{version_id}' has an empty inheritsFrom value")]
    EmptyParentVersion { version_id: String },
    #[error("version inheritance cycle detected at '{version_id}'")]
    InheritanceCycle { version_id: String },
    #[error("resolved version '{version_id}' does not declare mainClass")]
    MissingMainClass { version_id: String },
    #[error("version '{version_id}' declares both arguments.game and minecraftArguments")]
    ConflictingGameArguments { version_id: String },
    #[error("invalid minecraftArguments in version '{version_id}': {reason}")]
    InvalidLegacyArguments { version_id: String, reason: String },
    #[error("invalid Maven library coordinate '{coordinate}'")]
    InvalidLibraryCoordinate { coordinate: String },
    #[error("library '{library}' has no native classifier '{classifier}'")]
    MissingNativeClassifier { library: String, classifier: String },
    #[error("invalid launch artifact path '{path}'")]
    InvalidArtifactPath { path: String },
    #[error("arch_bits must be 32 or 64, got {arch_bits}")]
    InvalidArchitectureBits { arch_bits: u16 },
    #[error("invalid server launch metadata: {reason}")]
    InvalidServerMetadata { reason: String },
    #[error("launch argument references unresolved variable '{name}'")]
    UnresolvedVariable { name: String },
}

fn resolve_client_profiles(
    loader: LoaderKind,
    requested_id: &str,
    profiles: Vec<RawVersion>,
    environment: &LaunchEnvironment,
) -> Result<LaunchDescriptor, LaunchResolveError> {
    let inheritance_chain = profiles.iter().map(|profile| profile.id.clone()).collect();
    let mut main_class = None;
    let mut libraries = Vec::new();
    let mut natives: Vec<NativeArtifact> = Vec::new();
    let mut library_indices = BTreeMap::new();
    let mut jvm_arguments = Vec::new();
    let mut game_arguments = Vec::new();
    let mut assets = None;
    let mut asset_index = None;
    let mut java_version_major = None;
    let mut client_download = None;
    let mut client_jar_id = None;

    for profile in profiles {
        if let Some(value) = profile.main_class {
            main_class = Some(value);
        }
        if let Some(value) = profile.assets {
            assets = Some(value);
        }
        if let Some(value) = profile.asset_index {
            asset_index = Some(value.into_public());
        }
        if let Some(value) = profile.java_version {
            java_version_major = Some(value.major_version);
        }
        if let Some(download) = profile.downloads.client {
            client_download = Some(download);
            client_jar_id = Some(profile.id.clone());
        }
        if let Some(jar) = profile.jar {
            client_jar_id = Some(jar);
        }
        if profile.minecraft_arguments.is_some()
            && profile
                .arguments
                .as_ref()
                .is_some_and(|args| !args.game.is_empty())
        {
            return Err(LaunchResolveError::ConflictingGameArguments {
                version_id: profile.id,
            });
        }
        if let Some(arguments) = profile.arguments {
            jvm_arguments.extend(resolve_arguments(arguments.jvm, environment));
            game_arguments.extend(resolve_arguments(arguments.game, environment));
        }
        if let Some(arguments) = profile.minecraft_arguments {
            game_arguments = split_legacy_arguments(&profile.id, &arguments)?;
        }
        for library in profile.libraries {
            if !rules_allow(&library.rules, environment) {
                continue;
            }
            let coordinate = MavenCoordinate::parse(&library.name)?;
            let has_regular_artifact =
                library.downloads.artifact.is_some() || library.downloads.classifiers.is_empty();
            if has_regular_artifact {
                let artifact = resolve_library_artifact(&loader, &library)?;
                let library_identity = coordinate.identity();
                if let Some(index) = library_indices.get(&library_identity).copied() {
                    libraries[index] = artifact;
                } else {
                    library_indices.insert(library_identity, libraries.len());
                    libraries.push(artifact);
                }
                let replaced_module = maven_module_key(&library.name);
                natives.retain(|native| maven_module_key(&native.library_name) != replaced_module);
            }
            if let Some(classifier_template) = library.natives.get(&environment.os_name) {
                let classifier =
                    classifier_template.replace("${arch}", &environment.arch_bits.to_string());
                let native = resolve_native_artifact(&loader, &library, &classifier)?;
                let resolved = NativeArtifact {
                    library_name: library.name.clone(),
                    classifier,
                    artifact: native,
                    extract_excludes: library.extract.exclude.clone(),
                };
                if let Some(index) = natives.iter().position(|existing| {
                    maven_module_key(&existing.library_name) == maven_module_key(&library.name)
                        && existing.classifier == resolved.classifier
                }) {
                    natives[index] = resolved;
                } else {
                    natives.push(resolved);
                }
            }
        }
    }

    let main_class = main_class.ok_or_else(|| LaunchResolveError::MissingMainClass {
        version_id: requested_id.to_string(),
    })?;
    let client_jar = match (client_download, client_jar_id) {
        (Some(download), Some(jar_id)) => {
            let path = download
                .path
                .clone()
                .unwrap_or_else(|| format!("versions/{jar_id}/{jar_id}.jar"));
            Some(download.into_public(path))
        }
        (None, _) => None,
        (Some(_), None) => unreachable!("client download always records its source version"),
    };
    let classpath = unique_paths(
        libraries
            .iter()
            .map(|artifact| artifact.path.clone())
            .chain(client_jar.iter().map(|artifact| artifact.path.clone())),
    );
    validate_artifacts(&libraries)?;
    if let Some(jar) = &client_jar {
        validate_artifacts(std::slice::from_ref(jar))?;
    }
    Ok(LaunchDescriptor {
        loader,
        side: LaunchSide::Client,
        version_id: requested_id.to_string(),
        inheritance_chain,
        main_class,
        libraries,
        classpath,
        client_jar,
        jvm_arguments,
        game_arguments,
        assets,
        asset_index,
        natives,
        java_version_major,
    })
}

fn resolve_arguments(values: Vec<RawArgument>, environment: &LaunchEnvironment) -> Vec<String> {
    values
        .into_iter()
        .flat_map(|argument| match argument {
            RawArgument::Plain(value) => vec![value],
            RawArgument::Conditional { rules, value } if rules_allow(&rules, environment) => {
                value.into_values()
            }
            RawArgument::Conditional { .. } => Vec::new(),
        })
        .collect()
}

fn rules_allow(rules: &[RawRule], environment: &LaunchEnvironment) -> bool {
    if rules.is_empty() {
        return true;
    }
    let mut allowed = false;
    for rule in rules {
        if rule.matches(environment) {
            allowed = rule.action == RuleAction::Allow;
        }
    }
    allowed
}

fn resolve_library_artifact(
    loader: &LoaderKind,
    library: &RawLibrary,
) -> Result<LaunchArtifact, LaunchResolveError> {
    let coordinate = MavenCoordinate::parse(&library.name)?;
    let repository_path = library
        .downloads
        .artifact
        .as_ref()
        .and_then(|artifact| artifact.path.clone())
        .unwrap_or_else(|| coordinate.path());
    let (repository_path, path) = normalize_library_path(&repository_path)?;
    let download = library.downloads.artifact.as_ref();
    let url = download
        .and_then(|artifact| nonempty(artifact.url.clone()))
        .or_else(|| {
            let base = library
                .url
                .as_deref()
                .unwrap_or_else(|| default_library_base_url(loader, &coordinate));
            nonempty(Some(format!(
                "{}{repository_path}",
                ensure_trailing_slash(base)
            )))
        });
    Ok(LaunchArtifact {
        path,
        url,
        sha1: download
            .and_then(|artifact| artifact.sha1.clone())
            .or_else(|| library.sha1.clone()),
        size: download.and_then(|artifact| artifact.size).or(library.size),
    })
}

fn resolve_native_artifact(
    loader: &LoaderKind,
    library: &RawLibrary,
    classifier: &str,
) -> Result<LaunchArtifact, LaunchResolveError> {
    let coordinate = MavenCoordinate::parse(&library.name)?.with_classifier(classifier);
    let raw = library
        .downloads
        .classifiers
        .get(classifier)
        .ok_or_else(|| LaunchResolveError::MissingNativeClassifier {
            library: library.name.clone(),
            classifier: classifier.to_string(),
        })?;
    let repository_path = raw.path.clone().unwrap_or_else(|| coordinate.path());
    let (repository_path, path) = normalize_library_path(&repository_path)?;
    let url = nonempty(raw.url.clone()).or_else(|| {
        let base = library
            .url
            .as_deref()
            .unwrap_or_else(|| default_library_base_url(loader, &coordinate));
        nonempty(Some(format!(
            "{}{repository_path}",
            ensure_trailing_slash(base)
        )))
    });
    Ok(LaunchArtifact {
        path,
        url,
        sha1: raw.sha1.clone(),
        size: raw.size,
    })
}

fn default_library_base_url(loader: &LoaderKind, coordinate: &MavenCoordinate) -> &'static str {
    if loader == &LoaderKind::Cleanroom
        && coordinate.group == "com.cleanroommc"
        && coordinate.artifact == "cleanroom"
    {
        CLEANROOM_RELEASES_BASE_URL
    } else {
        DEFAULT_LIBRARY_BASE_URL
    }
}

fn normalize_library_path(path: &str) -> Result<(String, String), LaunchResolveError> {
    let normalized = path.replace('\\', "/");
    if Path::new(&normalized).is_absolute() {
        return Err(LaunchResolveError::InvalidArtifactPath {
            path: path.to_string(),
        });
    }
    let repository_path = normalized.strip_prefix("libraries/").unwrap_or(&normalized);
    if repository_path.is_empty()
        || repository_path.starts_with("libraries/")
        || repository_path.starts_with('/')
        || repository_path
            .split('/')
            .any(|component| component.is_empty() || matches!(component, "." | ".."))
    {
        return Err(LaunchResolveError::InvalidArtifactPath {
            path: path.to_string(),
        });
    }
    Ok((
        repository_path.to_string(),
        format!("libraries/{repository_path}"),
    ))
}

fn validate_environment(environment: &LaunchEnvironment) -> Result<(), LaunchResolveError> {
    if matches!(environment.arch_bits, 32 | 64) {
        Ok(())
    } else {
        Err(LaunchResolveError::InvalidArchitectureBits {
            arch_bits: environment.arch_bits,
        })
    }
}

fn validate_artifacts(artifacts: &[LaunchArtifact]) -> Result<(), LaunchResolveError> {
    for artifact in artifacts {
        if artifact.path.trim().is_empty() || Path::new(&artifact.path).is_absolute() {
            return Err(LaunchResolveError::InvalidArtifactPath {
                path: artifact.path.clone(),
            });
        }
    }
    Ok(())
}

fn unique_paths(paths: impl IntoIterator<Item = String>) -> Vec<String> {
    let mut seen = BTreeSet::new();
    paths
        .into_iter()
        .filter(|path| seen.insert(path.clone()))
        .collect()
}

fn maven_module_key(name: &str) -> (&str, &str) {
    let mut parts = name.split(':');
    let group = parts.next().unwrap_or_default();
    let artifact = parts.next().unwrap_or_default();
    (group, artifact)
}

fn split_legacy_arguments(
    version_id: &str,
    input: &str,
) -> Result<Vec<String>, LaunchResolveError> {
    let mut values = Vec::new();
    let mut current = String::new();
    let mut quote = None;
    let mut escaped = false;
    for character in input.chars() {
        if escaped {
            current.push(character);
            escaped = false;
            continue;
        }
        if character == '\\' {
            escaped = true;
            continue;
        }
        if matches!(character, '\'' | '"') {
            if quote == Some(character) {
                quote = None;
            } else if quote.is_none() {
                quote = Some(character);
            } else {
                current.push(character);
            }
            continue;
        }
        if character.is_whitespace() && quote.is_none() {
            if !current.is_empty() {
                values.push(std::mem::take(&mut current));
            }
        } else {
            current.push(character);
        }
    }
    if escaped || quote.is_some() {
        return Err(LaunchResolveError::InvalidLegacyArguments {
            version_id: version_id.to_string(),
            reason: "unterminated escape or quote".to_string(),
        });
    }
    if !current.is_empty() {
        values.push(current);
    }
    Ok(values)
}

fn substitute_all(
    arguments: &[String],
    variables: &BTreeMap<String, String>,
) -> Result<Vec<String>, LaunchResolveError> {
    arguments
        .iter()
        .map(|argument| substitute(argument, variables))
        .collect()
}

fn substitute(
    argument: &str,
    variables: &BTreeMap<String, String>,
) -> Result<String, LaunchResolveError> {
    let mut output = String::new();
    let mut remaining = argument;
    while let Some(start) = remaining.find("${") {
        output.push_str(&remaining[..start]);
        let variable = &remaining[start + 2..];
        let end = variable
            .find('}')
            .ok_or_else(|| LaunchResolveError::UnresolvedVariable {
                name: variable.to_string(),
            })?;
        let name = &variable[..end];
        let value = variables
            .get(name)
            .ok_or_else(|| LaunchResolveError::UnresolvedVariable {
                name: name.to_string(),
            })?;
        output.push_str(value);
        remaining = &variable[end + 1..];
    }
    output.push_str(remaining);
    Ok(output)
}

fn nonempty(value: Option<String>) -> Option<String> {
    value.filter(|value| !value.trim().is_empty())
}

fn ensure_trailing_slash(value: &str) -> String {
    if value.ends_with('/') {
        value.to_string()
    } else {
        format!("{value}/")
    }
}

#[derive(Debug, Clone)]
struct MavenCoordinate {
    group: String,
    artifact: String,
    version: String,
    classifier: Option<String>,
    extension: String,
}

impl MavenCoordinate {
    fn parse(raw: &str) -> Result<Self, LaunchResolveError> {
        let (coordinate, extension) = raw
            .rsplit_once('@')
            .map_or((raw, "jar"), |(left, right)| (left, right));
        let parts = coordinate.split(':').collect::<Vec<_>>();
        if !(3..=4).contains(&parts.len())
            || parts.iter().any(|part| part.is_empty())
            || extension.is_empty()
        {
            return Err(LaunchResolveError::InvalidLibraryCoordinate {
                coordinate: raw.to_string(),
            });
        }
        Ok(Self {
            group: parts[0].to_string(),
            artifact: parts[1].to_string(),
            version: parts[2].to_string(),
            classifier: parts.get(3).map(|value| (*value).to_string()),
            extension: extension.to_string(),
        })
    }

    fn with_classifier(mut self, classifier: &str) -> Self {
        self.classifier = Some(classifier.to_string());
        self
    }

    fn identity(&self) -> String {
        let classifier = self
            .classifier
            .as_deref()
            .map(|value| format!(":{value}"))
            .unwrap_or_default();
        format!(
            "{}:{}{classifier}@{}",
            self.group, self.artifact, self.extension
        )
    }

    fn path(&self) -> String {
        let classifier = self
            .classifier
            .as_ref()
            .map(|classifier| format!("-{classifier}"))
            .unwrap_or_default();
        format!(
            "{}/{}/{}/{}-{}{}.{}",
            self.group.replace('.', "/"),
            self.artifact,
            self.version,
            self.artifact,
            self.version,
            classifier,
            self.extension
        )
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawVersion {
    id: String,
    inherits_from: Option<String>,
    main_class: Option<String>,
    jar: Option<String>,
    assets: Option<String>,
    asset_index: Option<RawAssetIndex>,
    java_version: Option<RawJavaVersion>,
    #[serde(default)]
    downloads: RawVersionDownloads,
    arguments: Option<RawArguments>,
    minecraft_arguments: Option<String>,
    #[serde(default)]
    libraries: Vec<RawLibrary>,
}

#[derive(Debug, Default, Deserialize)]
struct RawVersionDownloads {
    client: Option<RawArtifact>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawAssetIndex {
    id: String,
    url: String,
    sha1: String,
    size: u64,
    total_size: u64,
}

impl RawAssetIndex {
    fn into_public(self) -> AssetIndexMetadata {
        AssetIndexMetadata {
            id: self.id,
            url: self.url,
            sha1: self.sha1,
            size: self.size,
            total_size: self.total_size,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawJavaVersion {
    major_version: u16,
}

#[derive(Debug, Deserialize)]
struct RawArguments {
    #[serde(default)]
    game: Vec<RawArgument>,
    #[serde(default)]
    jvm: Vec<RawArgument>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum RawArgument {
    Plain(String),
    Conditional {
        rules: Vec<RawRule>,
        value: RawArgumentValue,
    },
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum RawArgumentValue {
    One(String),
    Many(Vec<String>),
}

impl RawArgumentValue {
    fn into_values(self) -> Vec<String> {
        match self {
            Self::One(value) => vec![value],
            Self::Many(values) => values,
        }
    }
}

#[derive(Debug, Deserialize)]
struct RawLibrary {
    name: String,
    url: Option<String>,
    sha1: Option<String>,
    size: Option<u64>,
    #[serde(default)]
    downloads: RawLibraryDownloads,
    #[serde(default)]
    rules: Vec<RawRule>,
    #[serde(default)]
    natives: BTreeMap<String, String>,
    #[serde(default)]
    extract: RawExtract,
}

#[derive(Debug, Default, Deserialize)]
struct RawLibraryDownloads {
    artifact: Option<RawArtifact>,
    #[serde(default)]
    classifiers: BTreeMap<String, RawArtifact>,
}

#[derive(Debug, Clone, Deserialize)]
struct RawArtifact {
    path: Option<String>,
    url: Option<String>,
    sha1: Option<String>,
    size: Option<u64>,
}

impl RawArtifact {
    fn into_public(self, path: String) -> LaunchArtifact {
        LaunchArtifact {
            path,
            url: nonempty(self.url),
            sha1: self.sha1,
            size: self.size,
        }
    }
}

#[derive(Debug, Default, Deserialize)]
struct RawExtract {
    #[serde(default)]
    exclude: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct RawRule {
    action: RuleAction,
    os: Option<RawOsRule>,
    #[serde(default)]
    features: BTreeMap<String, bool>,
}

impl RawRule {
    fn matches(&self, environment: &LaunchEnvironment) -> bool {
        let os_matches = self.os.as_ref().is_none_or(|os| {
            os.name
                .as_ref()
                .is_none_or(|name| name == &environment.os_name)
                && os
                    .arch
                    .as_ref()
                    .is_none_or(|arch| arch == &environment.os_arch)
                && os
                    .version
                    .as_ref()
                    .is_none_or(|version| environment.matching_os_version_rules.contains(version))
        });
        os_matches
            && self.features.iter().all(|(name, expected)| {
                environment.features.get(name).copied().unwrap_or(false) == *expected
            })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
enum RuleAction {
    Allow,
    Disallow,
}

#[derive(Debug, Deserialize)]
struct RawOsRule {
    name: Option<String>,
    arch: Option<String>,
    version: Option<String>,
}
