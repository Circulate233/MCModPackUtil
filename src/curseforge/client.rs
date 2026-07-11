use std::collections::BTreeSet;

use reqwest::blocking::Client;
use serde::{Deserialize, Deserializer, de::Error};

use crate::curseforge::model::{
    CfAuthor, CfFile, CfProject, FileDependency, FileHash, ReleaseType, SideSupport,
};
use crate::curseforge::resolver::{CurseForgeApi, ResolveError};

const CURSEFORGE_API_BASE: &str = "https://api.curseforge.com";

pub struct CurseForgeClient {
    http: Client,
    base_url: String,
    api_key: String,
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

    pub fn with_base_url_and_proxy(base_url: String, api_key: String, proxy: Option<&str>) -> Self {
        Self::try_with_base_url_and_proxy(base_url, api_key, proxy)
            .expect("valid CurseForge proxy configuration")
    }

    pub fn try_with_base_url_and_proxy(
        base_url: String,
        api_key: String,
        proxy: Option<&str>,
    ) -> Result<Self, ResolveError> {
        let mut builder = Client::builder();
        if let Some(proxy) = proxy.map(str::trim).filter(|proxy| !proxy.is_empty()) {
            builder = builder.proxy(
                reqwest::Proxy::all(proxy).map_err(|err| ResolveError::Api(err.to_string()))?,
            );
        }
        Ok(Self {
            http: builder
                .build()
                .map_err(|err| ResolveError::Api(err.to_string()))?,
            base_url: base_url.trim_end_matches('/').to_string(),
            api_key,
        })
    }

    fn get_json<T: for<'de> Deserialize<'de>>(
        &self,
        path: &str,
        query: &[(&str, String)],
    ) -> Result<T, ResolveError> {
        let url = format!("{}{}", self.base_url, path);
        let response = self
            .http
            .get(url)
            .header("x-api-key", &self.api_key)
            .query(query)
            .send()
            .map_err(|err| ResolveError::Api(err.to_string()))?;
        let status = response.status();
        if !status.is_success() {
            let body = response.text().unwrap_or_default();
            return Err(ResolveError::Api(format!(
                "CurseForge returned {status}: {body}"
            )));
        }
        response
            .json()
            .map_err(|err| ResolveError::Api(err.to_string()))
    }
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
