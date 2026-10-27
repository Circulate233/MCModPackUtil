use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::curseforge::model::{CfFile, CfProject};

#[derive(Debug, Error)]
pub enum DependencyError {
    #[error("unsupported CurseForge class id {0}")]
    UnsupportedClass(u64),
    #[error("missing client/server side metadata for project {project_id} file {file_id}")]
    MissingSide { project_id: u64, file_id: u64 },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DependencySide {
    Client,
    Server,
    Common,
    Resourcepack,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DependencyType {
    #[serde(rename = "mod")]
    Mod,
    ResourcePack,
    ShaderPack,
}

impl DependencyType {
    pub fn from_curseforge_class(class_id: u64) -> Result<Self, DependencyError> {
        match class_id {
            6 => Ok(Self::Mod),
            12 => Ok(Self::ResourcePack),
            6552 => Ok(Self::ShaderPack),
            id => Err(DependencyError::UnsupportedClass(id)),
        }
    }

    /// Returns the CurseForge web category used in project and file page URLs.
    pub fn curseforge_web_category(&self) -> &'static str {
        match self {
            Self::Mod => "mc-mods",
            Self::ResourcePack => "texture-packs",
            Self::ShaderPack => "shaders",
        }
    }

    /// Builds a direct, user-facing CurseForge file page URL from known project metadata.
    pub fn curseforge_file_page_url(&self, slug: &str, file_id: u64) -> String {
        format!(
            "https://www.curseforge.com/minecraft/{}/{slug}/files/{file_id}",
            self.curseforge_web_category()
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DependencyEntry {
    pub project_id: Option<u64>,
    pub file_id: Option<u64>,
    pub slug: Option<String>,
    pub loader: Option<String>,
    #[serde(default = "default_dependency_side", skip_serializing)]
    pub side: DependencySide,
    #[serde(rename = "type")]
    pub dependency_type: DependencyType,
    pub name: String,
    #[serde(default, skip_serializing)]
    pub authors: Vec<String>,
    pub required: bool,
}

impl DependencyEntry {
    pub fn from_curseforge(project: &CfProject, file: &CfFile) -> Result<Self, DependencyError> {
        let dependency_type = DependencyType::from_curseforge_class(project.class_id)?;
        let side = match dependency_type {
            DependencyType::ResourcePack | DependencyType::ShaderPack => {
                DependencySide::Resourcepack
            }
            DependencyType::Mod => match side_from_file_or_project(project, file) {
                Some(side) => side,
                None => {
                    return Err(DependencyError::MissingSide {
                        project_id: project.id,
                        file_id: file.id,
                    });
                }
            },
        };

        Ok(Self {
            project_id: Some(project.id),
            file_id: Some(file.id),
            slug: Some(project.slug.clone()),
            loader: file.loader(),
            side,
            dependency_type,
            name: project.name.clone(),
            authors: project.author_names(),
            required: true,
        })
    }

    pub fn from_curseforge_with_side(
        project: &CfProject,
        file: &CfFile,
        side: DependencySide,
    ) -> Result<Self, DependencyError> {
        let dependency_type = DependencyType::from_curseforge_class(project.class_id)?;
        Ok(Self {
            project_id: Some(project.id),
            file_id: Some(file.id),
            slug: Some(project.slug.clone()),
            loader: file.loader(),
            side,
            dependency_type,
            name: project.name.clone(),
            authors: project.author_names(),
            required: true,
        })
    }
}

fn default_dependency_side() -> DependencySide {
    DependencySide::Common
}

fn side_from_file_or_project(project: &CfProject, file: &CfFile) -> Option<DependencySide> {
    match (file.has_client_tag(), file.has_server_tag()) {
        (true, true) => Some(DependencySide::Common),
        (true, false) => Some(DependencySide::Client),
        (false, true) => Some(DependencySide::Server),
        (false, false) => {
            match (
                project.has_client_side_support(),
                project.has_server_side_support(),
            ) {
                (true, true) => Some(DependencySide::Common),
                (true, false) => Some(DependencySide::Client),
                (false, true) => Some(DependencySide::Server),
                (false, false) => None,
            }
        }
    }
}
