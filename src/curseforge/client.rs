use std::collections::BTreeSet;

use reqwest::Url;
use reqwest::header::{HeaderName, HeaderValue};
use serde::{Deserialize, Deserializer, de::Error};

use crate::curseforge::model::{
    CfAuthor, CfFile, CfProject, FileDependency, FileHash, ReleaseType, SideSupport,
};
use crate::curseforge::resolver::{CurseForgeApi, ResolveError};
use crate::net::{HttpRequest, HttpTransport, NetworkConfig, NetworkEngine, SensitiveHeaders};

const CURSEFORGE_API_BASE: &str = "https://api.curseforge.com";
const CURSEFORGE_API_RESPONSE_LIMIT: usize = 16 * 1024 * 1024;

#[derive(Clone)]
pub struct CurseForgeClient {
    network: NetworkEngine,
    base_url: Url,
    api_key: SensitiveHeaders,
}

impl CurseForgeClient {
    pub fn new(api_key: String) -> Self {
        Self::with_base_url(CURSEFORGE_API_BASE.to_string(), api_key)
    }

    pub fn with_base_url(base_url: String, api_key: String) -> Self {
        Self::with_base_url_and_proxy(base_url, api_key, None)
    }

    pub fn with_proxy(api_key: String, proxy: Option<&str>) -> Result<Self, ResolveError> {
        Self::try_with_base_url_and_proxy(CURSEFORGE_API_BASE.to_string(), api_key, proxy)
    }

    /// Creates a client on an existing shared network session.
    pub fn with_network_engine(
        api_key: String,
        network: NetworkEngine,
    ) -> Result<Self, ResolveError> {
        Self::try_with_base_url_and_network(CURSEFORGE_API_BASE.to_string(), api_key, network)
    }

    pub fn with_base_url_and_proxy(base_url: String, api_key: String, proxy: Option<&str>) -> Self {
        Self::try_with_base_url_and_proxy(base_url, api_key, proxy)
            .expect("valid CurseForge proxy configuration")
    }

    pub fn try_with_base_url_and_proxy(
        base_url: String,
        api_key: String,
        proxy: Option<&str>,
    ) -> Result<Self, ResolveError> {
        let network = NetworkEngine::new(NetworkConfig {
            proxy: proxy
                .map(str::trim)
                .filter(|proxy| !proxy.is_empty())
                .map(ToOwned::to_owned),
            ..NetworkConfig::default()
        })
        .map_err(api_error)?;
        Self::try_with_base_url_and_network(base_url, api_key, network)
    }

    /// Creates a client for an explicit API origin on an existing network session.
    pub fn try_with_base_url_and_network(
        base_url: String,
        api_key: String,
        network: NetworkEngine,
    ) -> Result<Self, ResolveError> {
        let base_url = parse_base_url(&base_url)?;
        let api_key = SensitiveHeaders::new()
            .allow_origin(base_url.as_str())
            .and_then(|headers| {
                HeaderValue::from_str(&api_key)
                    .map_err(|source| crate::net::NetworkError::InvalidConfiguration {
                        reason: format!("invalid CurseForge API key header: {source}"),
                    })
                    .and_then(|value| headers.insert(HeaderName::from_static("x-api-key"), value))
            })
            .map_err(api_error)?;
        Ok(Self {
            network,
            base_url,
            api_key,
        })
    }

    fn get_json<T: for<'de> Deserialize<'de>>(
        &self,
        path: &str,
        query: &[(&str, String)],
    ) -> Result<T, ResolveError> {
        let mut url = self.base_url.clone();
        let base_path = self.base_url.path().trim_end_matches('/');
        url.set_path(&format!("{base_path}{path}"));
        url.set_query(None);
        if !query.is_empty() {
            url.query_pairs_mut()
                .extend_pairs(query.iter().map(|(name, value)| (*name, value.as_str())));
        }
        let request = HttpRequest::get(url.as_str(), CURSEFORGE_API_RESPONSE_LIMIT)
            .sensitive_headers(self.api_key.clone())
            .build()
            .map_err(api_error)?;
        self.network.get_json(request).map_err(api_error)
    }
}

fn parse_base_url(base_url: &str) -> Result<Url, ResolveError> {
    let normalized = base_url.trim_end_matches('/');
    let url = Url::parse(normalized).map_err(|source| {
        ResolveError::Api(format!(
            "invalid CurseForge API base URL '{base_url}': {source}"
        ))
    })?;
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
        return Err(ResolveError::Api(format!(
            "invalid CurseForge API base URL '{base_url}': expected an absolute HTTP URL"
        )));
    }
    Ok(url)
}

fn api_error(error: impl std::fmt::Display) -> ResolveError {
    ResolveError::Api(error.to_string())
}

impl CurseForgeApi for CurseForgeClient {
    fn get_project(&self, project_id: u64) -> Result<CfProject, ResolveError> {
        let response: DataResponse<RawProject> =
            self.get_json(&format!("/v1/mods/{project_id}"), &[])?;
        Ok(response.data.into())
    }

    fn get_file(&self, project_id: u64, file_id: u64) -> Result<CfFile, ResolveError> {
        let response: DataResponse<RawFile> =
            self.get_json(&format!("/v1/mods/{project_id}/files/{file_id}"), &[])?;
        Ok(response.data.into())
    }

    fn find_project_by_slug(&self, slug: &str) -> Result<CfProject, ResolveError> {
        let response: DataResponse<Vec<RawProject>> = self.get_json(
            "/v1/mods/search",
            &[("gameId", "432".to_string()), ("slug", slug.to_string())],
        )?;
        match response.data.as_slice() {
            [project] => Ok(project.clone().into()),
            [] => Err(ResolveError::Api(format!(
                "no CurseForge project matched slug '{slug}'"
            ))),
            _ => Err(ResolveError::Api(format!(
                "multiple CurseForge projects matched slug '{slug}'"
            ))),
        }
    }

    fn search_projects(&self, query: &str) -> Result<Vec<CfProject>, ResolveError> {
        let response: DataResponse<Vec<RawProject>> = self.get_json(
            "/v1/mods/search",
            &[
                ("gameId", "432".to_string()),
                ("searchFilter", query.trim().to_string()),
                ("pageSize", "20".to_string()),
            ],
        )?;
        Ok(response.data.into_iter().map(Into::into).collect())
    }

    fn list_files(
        &self,
        project_id: u64,
        minecraft_version: &str,
        loader: &str,
    ) -> Result<Vec<CfFile>, ResolveError> {
        let mut query = Vec::new();
        if !minecraft_version.trim().is_empty() {
            query.push(("gameVersion", minecraft_version.to_string()));
        }
        if !loader.trim().is_empty() {
            query.push(("modLoaderType", loader_type(loader)?.to_string()));
        }
        let response: DataResponse<Vec<RawFile>> =
            self.get_json(&format!("/v1/mods/{project_id}/files"), &query)?;
        Ok(response.data.into_iter().map(Into::into).collect())
    }

    fn list_project_files(&self, project_id: u64) -> Result<Vec<CfFile>, ResolveError> {
        let response: DataResponse<Vec<RawFile>> =
            self.get_json(&format!("/v1/mods/{project_id}/files"), &[])?;
        Ok(response.data.into_iter().map(Into::into).collect())
    }
}

fn loader_type(loader: &str) -> Result<u8, ResolveError> {
    match loader.to_ascii_lowercase().as_str() {
        "forge" => Ok(1),
        "cauldron" => Ok(2),
        "liteloader" => Ok(3),
        "fabric" => Ok(4),
        "quilt" => Ok(5),
        "neoforge" => Ok(6),
        other => Err(ResolveError::Api(format!(
            "unsupported CurseForge loader '{other}'"
        ))),
    }
}

#[derive(Debug, Deserialize)]
struct DataResponse<T> {
    data: T,
}

#[derive(Debug, Clone, Deserialize)]
struct RawProject {
    id: u64,
    name: String,
    slug: String,
    #[serde(rename = "classId")]
    class_id: u64,
    #[serde(
        rename = "clientSide",
        default,
        deserialize_with = "deserialize_optional_side_support"
    )]
    client_side: Option<SideSupport>,
    #[serde(
        rename = "serverSide",
        default,
        deserialize_with = "deserialize_optional_side_support"
    )]
    server_side: Option<SideSupport>,
    #[serde(default)]
    authors: Vec<RawAuthor>,
}

#[derive(Debug, Clone, Deserialize)]
struct RawAuthor {
    name: String,
}

impl From<RawProject> for CfProject {
    fn from(value: RawProject) -> Self {
        Self {
            id: value.id,
            name: value.name,
            slug: value.slug,
            class_id: value.class_id,
            client_side: value.client_side,
            server_side: value.server_side,
            authors: value
                .authors
                .into_iter()
                .map(|author| CfAuthor { name: author.name })
                .collect(),
        }
    }
}

fn deserialize_optional_side_support<'de, D>(
    deserializer: D,
) -> Result<Option<SideSupport>, D::Error>
where
    D: Deserializer<'de>,
{
    let value = Option::<serde_json::Value>::deserialize(deserializer)?;
    let Some(value) = value else {
        return Ok(None);
    };
    match value {
        serde_json::Value::Null => Ok(None),
        serde_json::Value::Bool(true) => Ok(Some(SideSupport::Required)),
        serde_json::Value::Bool(false) => Ok(Some(SideSupport::Unsupported)),
        serde_json::Value::Number(number) => match number.as_u64() {
            Some(0) => Ok(None),
            Some(1) => Ok(Some(SideSupport::Required)),
            Some(2) => Ok(Some(SideSupport::Optional)),
            Some(3) => Ok(Some(SideSupport::Unsupported)),
            Some(other) => Err(D::Error::custom(format!(
                "unsupported CurseForge side metadata value {other}"
            ))),
            None => Err(D::Error::custom("invalid CurseForge side metadata number")),
        },
        serde_json::Value::String(raw) => match raw.trim().to_ascii_lowercase().as_str() {
            "" | "unknown" => Ok(None),
            "required" | "supported" | "true" => Ok(Some(SideSupport::Required)),
            "optional" => Ok(Some(SideSupport::Optional)),
            "unsupported" | "false" => Ok(Some(SideSupport::Unsupported)),
            other => Err(D::Error::custom(format!(
                "unsupported CurseForge side metadata value '{other}'"
            ))),
        },
        _ => Err(D::Error::custom("invalid CurseForge side metadata type")),
    }
}

#[derive(Debug, Deserialize)]
struct RawFile {
    id: u64,
    #[serde(rename = "modId")]
    mod_id: u64,
    #[serde(rename = "fileName")]
    file_name: String,
    #[serde(rename = "displayName")]
    display_name: Option<String>,
    #[serde(rename = "fileLength", default)]
    file_length: u64,
    #[serde(rename = "isAvailable")]
    is_available: bool,
    #[serde(rename = "fileStatus")]
    file_status: u8,
    #[serde(rename = "releaseType")]
    release_type: u8,
    #[serde(rename = "gameVersions")]
    game_versions: BTreeSet<String>,
    #[serde(default)]
    dependencies: Vec<RawDependency>,
    #[serde(rename = "downloadUrl")]
    download_url: Option<String>,
    #[serde(default)]
    hashes: Vec<FileHash>,
}

#[derive(Debug, Deserialize)]
struct RawDependency {
    #[serde(rename = "modId")]
    mod_id: u64,
    #[serde(rename = "relationType")]
    relation_type: u8,
}

impl From<RawFile> for CfFile {
    fn from(value: RawFile) -> Self {
        Self {
            id: value.id,
            mod_id: value.mod_id,
            file_name: value.file_name,
            display_name: value.display_name,
            file_length: value.file_length,
            is_available: value.is_available,
            file_status: value.file_status,
            release_type: match value.release_type {
                1 => ReleaseType::Stable,
                2 => ReleaseType::Beta,
                3 => ReleaseType::Alpha,
                _ => ReleaseType::Alpha,
            },
            game_versions: value.game_versions,
            dependencies: value
                .dependencies
                .into_iter()
                .map(|dependency| FileDependency {
                    mod_id: dependency.mod_id,
                    relation_type: dependency.relation_type,
                })
                .collect(),
            download_url: value.download_url,
            hashes: value.hashes,
        }
    }
}
