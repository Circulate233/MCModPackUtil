use serde::{Deserialize, Serialize};

use crate::server::loader::LoaderInstallerPlan;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerFile {
    pub path: String,
    pub url: String,
    pub sha1: String,
    pub size: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerFilesManifest {
    pub language: String,
    pub curseforge_api_key: String,
    pub java: ServerJavaConfig,
    pub files: Vec<ServerFile>,
    pub loader: LoaderInstallerPlan,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerJavaConfig {
    pub min_memory: String,
    pub max_memory: String,
    pub extra_args: Vec<String>,
}
