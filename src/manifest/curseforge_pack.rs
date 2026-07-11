use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ManifestError {
    #[error("failed to parse manifest.json: {0}")]
    Json(#[from] serde_json::Error),
    #[error("manifest.version contains illegal placeholder value '{0}'")]
    IllegalVersion(String),
    #[error("manifest.minecraft.modLoaders has no primary loader")]
    MissingPrimaryLoader,
    #[error("manifest.minecraft.modLoaders primary id '{0}' is invalid")]
    InvalidPrimaryLoader(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImportedManifest {
    pub author: String,
    pub files: Vec<ManifestFile>,
    #[serde(rename = "manifestType")]
    pub manifest_type: String,
    #[serde(rename = "manifestVersion")]
    pub manifest_version: u32,
    pub minecraft: MinecraftSection,
    pub name: String,
    pub overrides: String,
    pub version: String,
}

impl ImportedManifest {
    pub fn from_slice(bytes: &[u8]) -> Result<Self, ManifestError> {
        let manifest: Self = serde_json::from_slice(bytes)?;
        let trimmed = manifest.version.trim();
        if trimmed.is_empty() || trimmed.contains("${") {
            return Err(ManifestError::IllegalVersion(manifest.version));
        }
        Ok(manifest)
    }

    pub fn authors(&self) -> Vec<String> {
        self.author
            .split(',')
            .map(str::trim)
            .filter(|author| !author.is_empty())
            .map(ToOwned::to_owned)
            .collect()
    }

    pub fn primary_loader_parts(&self) -> (String, String) {
        self.try_primary_loader_parts()
            .expect("imported manifest must contain a valid primary loader")
    }

    pub fn try_primary_loader_parts(&self) -> Result<(String, String), ManifestError> {
        let loader = self
            .minecraft
            .mod_loaders
            .iter()
            .find(|loader| loader.primary)
            .or_else(|| self.minecraft.mod_loaders.first())
            .ok_or(ManifestError::MissingPrimaryLoader)?;
        let (name, version) = loader
            .id
            .split_once('-')
            .ok_or_else(|| ManifestError::InvalidPrimaryLoader(loader.id.clone()))?;
        Ok((name.to_ascii_lowercase(), version.to_string()))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManifestFile {
    #[serde(rename = "projectID")]
    pub project_id: u64,
    #[serde(rename = "fileID")]
    pub file_id: u64,
    pub required: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MinecraftSection {
    #[serde(rename = "modLoaders")]
    pub mod_loaders: Vec<ModLoaderEntry>,
    pub version: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModLoaderEntry {
    pub id: String,
    pub primary: bool,
}
