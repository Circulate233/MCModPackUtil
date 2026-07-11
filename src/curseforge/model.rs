use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReleaseType {
    Stable,
    Beta,
    Alpha,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileHash {
    pub value: String,
    pub algo: u8,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileDependency {
    pub mod_id: u64,
    pub relation_type: u8,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CfFile {
    pub id: u64,
    pub mod_id: u64,
    pub file_name: String,
    pub display_name: Option<String>,
    pub file_length: u64,
    pub is_available: bool,
    pub file_status: u8,
    pub release_type: ReleaseType,
    pub game_versions: BTreeSet<String>,
    pub dependencies: Vec<FileDependency>,
    pub download_url: Option<String>,
    pub hashes: Vec<FileHash>,
}

impl CfFile {
    pub fn missing_required_dependencies(&self, existing_projects: &BTreeSet<u64>) -> Vec<u64> {
        self.dependencies
            .iter()
            .filter(|dependency| dependency.relation_type == 3)
            .map(|dependency| dependency.mod_id)
            .filter(|mod_id| !existing_projects.contains(mod_id))
            .collect()
    }

    pub fn loader(&self) -> Option<String> {
        const LOADERS: &[&str] = &["forge", "fabric", "neoforge", "quilt", "liteloader"];
        self.game_versions
            .iter()
            .map(|version| version.to_ascii_lowercase())
            .find(|version| LOADERS.contains(&version.as_str()))
    }

    pub fn has_client_tag(&self) -> bool {
        self.game_versions
            .iter()
            .any(|version| version.eq_ignore_ascii_case("client"))
    }

    pub fn has_server_tag(&self) -> bool {
        self.game_versions
            .iter()
            .any(|version| version.eq_ignore_ascii_case("server"))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CfAuthor {
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CfProject {
    pub id: u64,
    pub name: String,
    pub slug: String,
    pub class_id: u64,
    pub client_side: Option<SideSupport>,
    pub server_side: Option<SideSupport>,
    pub authors: Vec<CfAuthor>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SideSupport {
    Required,
    Optional,
    Unsupported,
}

impl SideSupport {
    pub fn is_supported(self) -> bool {
        matches!(self, Self::Required | Self::Optional)
    }
}

impl CfProject {
    pub fn new_for_test(
        id: u64,
        name: impl Into<String>,
        slug: impl Into<String>,
        class_id: u64,
        authors: Vec<&str>,
    ) -> Self {
        Self {
            id,
            name: name.into(),
            slug: slug.into(),
            class_id,
            client_side: None,
            server_side: None,
            authors: authors
                .into_iter()
                .map(|name| CfAuthor {
                    name: name.to_string(),
                })
                .collect(),
        }
    }

    pub fn author_names(&self) -> Vec<String> {
        self.authors
            .iter()
            .map(|author| author.name.clone())
            .collect()
    }

    pub fn with_side_support(
        mut self,
        client_side: Option<SideSupport>,
        server_side: Option<SideSupport>,
    ) -> Self {
        self.client_side = client_side;
        self.server_side = server_side;
        self
    }

    pub fn has_client_side_support(&self) -> bool {
        self.client_side
            .map(SideSupport::is_supported)
            .unwrap_or(false)
    }

    pub fn has_server_side_support(&self) -> bool {
        self.server_side
            .map(SideSupport::is_supported)
            .unwrap_or(false)
    }
}
