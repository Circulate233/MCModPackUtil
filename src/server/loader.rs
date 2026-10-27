use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::config::LoaderKind;

/// Existing loader installer plan consumed by local client/server runtime installation.
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

/// Loader family, official installer, and exact output serialized to `server-install.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerInstallLoader {
    /// Loader family understood by MCServerDownloadTool.
    pub kind: ServerLoaderKind,
    /// Exact configured loader version.
    pub version: String,
    /// Official installer artifact and SHA-1 sidecar.
    pub installer: LoaderInstallerArtifact,
    /// Exact output that must exist after installer success.
    pub output: LoaderOutput,
}

/// Loader families supported by server-install schema v1.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ServerLoaderKind {
    Forge,
    Fabric,
    NeoForge,
    Cleanroom,
}

/// Official loader installer declaration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LoaderInstallerArtifact {
    /// Official installer JAR URL.
    pub url: String,
    /// Same-origin SHA-1 sidecar URL.
    pub sha1_sidecar: String,
}

/// Exact loader output expected after installation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum LoaderOutput {
    ModernArgs {
        windows: String,
        unix: String,
    },
    ExactJar {
        path: String,
        main_class: Option<String>,
    },
}

/// Fail-fast loader planning errors shared by runtime and server-manifest planning.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum LoaderPlanError {
    #[error("loader version must not be empty")]
    EmptyLoaderVersion,
    #[error("Minecraft version '{version}' must contain two or three numeric components")]
    InvalidMinecraftVersion { version: String },
}

/// Preserves the existing installer plan contract used by local runtime installation.
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
            installer_url: forge_installer_url(minecraft_version, loader_version),
            installer_sha1: None,
        },
        LoaderKind::Fabric => LoaderInstallerPlan::Fabric {
            installer_url: fabric_installer_url().to_string(),
            minecraft_version: minecraft_version.to_string(),
            loader_version: loader_version.to_string(),
        },
        LoaderKind::NeoForge => LoaderInstallerPlan::NeoForge {
            installer_url: neoforge_installer_url(loader_version),
            installer_sha1: None,
        },
        LoaderKind::Cleanroom => LoaderInstallerPlan::Cleanroom {
            installer_url: cleanroom_installer_url(loader_version),
            installer_sha1: None,
        },
    })
}

/// Builds the MCServerDownloadTool schema-v1 loader declaration without changing runtime plans.
pub fn plan_server_install_loader(
    loader: &LoaderKind,
    minecraft_version: &str,
    loader_version: &str,
) -> Result<ServerInstallLoader, LoaderPlanError> {
    if loader_version.trim().is_empty() {
        return Err(LoaderPlanError::EmptyLoaderVersion);
    }
    let (kind, installer_url) = match loader {
        LoaderKind::Forge => (
            ServerLoaderKind::Forge,
            forge_installer_url(minecraft_version, loader_version),
        ),
        LoaderKind::Fabric => (ServerLoaderKind::Fabric, fabric_installer_url().to_string()),
        LoaderKind::NeoForge => (
            ServerLoaderKind::NeoForge,
            neoforge_installer_url(loader_version),
        ),
        LoaderKind::Cleanroom => (
            ServerLoaderKind::Cleanroom,
            cleanroom_installer_url(loader_version),
        ),
    };
    let output = match loader {
        LoaderKind::Fabric => LoaderOutput::ExactJar {
            path: "fabric-server-launch.jar".to_string(),
            main_class: Some(
                "net.fabricmc.loader.impl.launch.server.FabricServerLauncher".to_string(),
            ),
        },
        LoaderKind::NeoForge => {
            let base = format!("libraries/net/neoforged/neoforge/{loader_version}");
            LoaderOutput::ModernArgs {
                windows: format!("{base}/win_args.txt"),
                unix: format!("{base}/unix_args.txt"),
            }
        }
        LoaderKind::Cleanroom => LoaderOutput::ExactJar {
            // Cleanroom's installer publishes the runnable jar using the
            // Cleanroom loader version, not the Minecraft version.
            path: format!("cleanroom-{loader_version}.jar"),
            main_class: None,
        },
        LoaderKind::Forge if is_modern_forge(minecraft_version)? => {
            let base =
                format!("libraries/net/minecraftforge/forge/{minecraft_version}-{loader_version}");
            LoaderOutput::ModernArgs {
                windows: format!("{base}/win_args.txt"),
                unix: format!("{base}/unix_args.txt"),
            }
        }
        LoaderKind::Forge => LoaderOutput::ExactJar {
            path: format!("forge-{minecraft_version}-{loader_version}.jar"),
            main_class: None,
        },
    };
    Ok(ServerInstallLoader {
        kind,
        version: loader_version.to_string(),
        installer: LoaderInstallerArtifact {
            sha1_sidecar: format!("{installer_url}.sha1"),
            url: installer_url,
        },
        output,
    })
}

fn forge_installer_url(minecraft_version: &str, loader_version: &str) -> String {
    format!(
        "https://maven.minecraftforge.net/net/minecraftforge/forge/{minecraft_version}-{loader_version}/forge-{minecraft_version}-{loader_version}-installer.jar"
    )
}

fn fabric_installer_url() -> &'static str {
    "https://maven.fabricmc.net/net/fabricmc/fabric-installer/1.1.1/fabric-installer-1.1.1.jar"
}

fn neoforge_installer_url(loader_version: &str) -> String {
    format!(
        "https://maven.neoforged.net/releases/net/neoforged/neoforge/{loader_version}/neoforge-{loader_version}-installer.jar"
    )
}

fn cleanroom_installer_url(loader_version: &str) -> String {
    format!(
        "https://repo.cleanroommc.com/{}/com/cleanroommc/cleanroom/{loader_version}/cleanroom-{loader_version}-installer.jar",
        cleanroom_repository(loader_version)
    )
}

fn is_modern_forge(version: &str) -> Result<bool, LoaderPlanError> {
    let components = version.split('.').collect::<Vec<_>>();
    if !(2..=3).contains(&components.len())
        || components.iter().any(|component| {
            component.is_empty() || !component.bytes().all(|byte| byte.is_ascii_digit())
        })
    {
        return Err(LoaderPlanError::InvalidMinecraftVersion {
            version: version.to_string(),
        });
    }
    let parse = |component: &str| {
        component
            .parse::<u32>()
            .map_err(|_| LoaderPlanError::InvalidMinecraftVersion {
                version: version.to_string(),
            })
    };
    Ok((parse(components[0])?, parse(components[1])?) >= (1, 17))
}

fn cleanroom_repository(loader_version: &str) -> &'static str {
    if loader_version.to_ascii_lowercase().contains("snapshot") {
        "snapshots"
    } else {
        "releases"
    }
}
