use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::config::LoaderKind;

#[derive(Debug, Error)]
pub enum LoaderPlanError {
    #[error("loader version must not be empty")]
    EmptyLoaderVersion,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum LoaderInstallerPlan {
    Forge {
        installer_url: String,
        installer_sha1: Option<String>,
    },
    Fabric {
        installer_url: String,
        minecraft_version: String,
        loader_version: String,
    },
    NeoForge {
        installer_url: String,
        installer_sha1: Option<String>,
    },
    Cleanroom {
        installer_url: String,
        installer_sha1: Option<String>,
    },
}

pub fn plan_loader_install(
    loader: &LoaderKind,
    minecraft_version: &str,
    loader_version: &str,
) -> Result<LoaderInstallerPlan, LoaderPlanError> {
    if loader_version.trim().is_empty() {
        return Err(LoaderPlanError::EmptyLoaderVersion);
    }

    Ok(match loader {
        LoaderKind::Forge => LoaderInstallerPlan::Forge {
            installer_url: format!(
                "https://maven.minecraftforge.net/net/minecraftforge/forge/{minecraft_version}-{loader_version}/forge-{minecraft_version}-{loader_version}-installer.jar"
            ),
            installer_sha1: None,
        },
        LoaderKind::Fabric => LoaderInstallerPlan::Fabric {
            installer_url: "https://meta.fabricmc.net/v2/versions/installer".to_string(),
            minecraft_version: minecraft_version.to_string(),
            loader_version: loader_version.to_string(),
        },
        LoaderKind::NeoForge => LoaderInstallerPlan::NeoForge {
            installer_url: format!(
                "https://maven.neoforged.net/releases/net/neoforged/neoforge/{loader_version}/neoforge-{loader_version}-installer.jar"
            ),
            installer_sha1: None,
        },
        LoaderKind::Cleanroom => LoaderInstallerPlan::Cleanroom {
            installer_url: format!(
                "https://maven.cleanroommc.com/com/cleanroommc/cleanroom/{loader_version}/cleanroom-{loader_version}-installer.jar"
            ),
            installer_sha1: None,
        },
    })
}
