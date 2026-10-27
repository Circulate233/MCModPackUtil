#![allow(dead_code)]

use mcmpu::config::{
    BuildConfig, CleanroomConfig, CleanroomRelauncher, Config, JavaConfig, JavaMemoryConfig,
    LoaderKind, NetworkConfig, PackConfig, TestRunConfig,
};
use mcmpu::curseforge::model::{CfFile, CfProject, FileHash, ReleaseType};
use mcmpu::curseforge::resolver::{CurseForgeApi, ResolveError};
use mcmpu::dependency::{DependencyEntry, DependencySide, DependencyType};
use mcmpu::project::cleanroom_relauncher::{MmcPackSource, mmc_pack_release_url};
use mcmpu::server::installer::{ServerInstallerAsset, ServerInstallerSource, ServerPlatform};
use sha1::{Digest, Sha1};
use sha2::Sha256;
use std::fs::{self, File};
use std::io::{Cursor, Write};
use std::path::Path;
use zip::write::SimpleFileOptions;

pub fn sample_config() -> Config {
    Config {
        language: "zh-CN".to_string(),
        pack: PackConfig {
            name: "ExamplePack-cleanroom".to_string(),
            version: "1.0.0".to_string(),
            authors: vec!["ExampleAuthor".to_string(), "SecondAuthor".to_string()],
            minecraft_version: "1.12.2".to_string(),
            loader: LoaderKind::Cleanroom,
            loader_version: "1.0.0".to_string(),
            forge_compat_version: Some("1.0.0".to_string()),
        },
        network: NetworkConfig::default(),
        build: BuildConfig {
            output_dir: "dist".to_string(),
        },
        java: JavaConfig {
            version_major: 8,
            client: JavaMemoryConfig {
                min_memory: "2G".to_string(),
                max_memory: "4G".to_string(),
                extra_args: Vec::new(),
            },
            server: JavaMemoryConfig {
                min_memory: "2G".to_string(),
                max_memory: "4G".to_string(),
                extra_args: vec!["-XX:+UseG1GC".to_string()],
            },
        },
        test_run: TestRunConfig::default(),
        cleanroom: Some(CleanroomConfig {
            use_scalar: false,
            scalar_version: 2,
            relauncher: CleanroomRelauncher::Unofficial,
        }),
    }
}

pub fn write_empty_manifest_zip(path: &Path) {
    write_zip(
        path,
        &[
            ("manifest.json", empty_manifest().as_bytes()),
            ("overrides/config/example.cfg", b"enabled=true".as_slice()),
        ],
    );
}

pub fn write_zip(path: &Path, entries: &[(&str, &[u8])]) {
    let file = File::create(path).unwrap();
    let mut zip = zip::ZipWriter::new(file);
    let options = SimpleFileOptions::default();
    for (name, bytes) in entries {
        zip.start_file(name, options).unwrap();
        zip.write_all(bytes).unwrap();
    }
    zip.finish().unwrap();
}

pub fn empty_manifest() -> String {
    r#"{
      "manifestType": "minecraftModpack",
      "manifestVersion": 1,
      "name": "ExamplePack-cleanroom",
      "version": "1.0.0",
      "author": "ExampleAuthor,SecondAuthor",
      "overrides": "overrides",
      "minecraft": {
        "version": "1.12.2",
        "modLoaders": [{"id": "forge-1.0.0", "primary": true}]
      },
      "files": []
    }"#
    .to_string()
}

pub struct FixtureApi;

pub struct FixtureMmcPackSource;

pub struct FixtureServerInstallerSource;

impl ServerInstallerSource for FixtureServerInstallerSource {
    type Error = std::io::Error;

    fn prepare(
        &self,
        project_dir: &Path,
        platform: ServerPlatform,
    ) -> Result<ServerInstallerAsset, Self::Error> {
        let fixture_dir = project_dir.join(".mcmpu/test-server-installer-assets");
        fs::create_dir_all(&fixture_dir)?;
        let (name, bytes) = match platform {
            ServerPlatform::LinuxX86_64 => (
                "MCServerDownloadTool-linux-x86_64",
                b"fixture-linux-installer".as_slice(),
            ),
            ServerPlatform::MacosAarch64 => (
                "MCServerDownloadTool-macos-aarch64",
                b"fixture-macos-installer".as_slice(),
            ),
            ServerPlatform::WindowsX86_64 => (
                "MCServerDownloadTool-windows-x86_64.exe",
                b"fixture-windows-installer".as_slice(),
            ),
        };
        let path = fixture_dir.join(name);
        fs::write(&path, bytes)?;
        ServerInstallerAsset::new(
            platform,
            path,
            bytes.len() as u64,
            format!("{:x}", Sha256::digest(bytes)),
        )
        .map_err(std::io::Error::other)
    }
}

impl MmcPackSource for FixtureMmcPackSource {
    type Error = std::io::Error;

    fn fetch(&self, url: &str) -> Result<Vec<u8>, Self::Error> {
        let without_sha1 = url.strip_suffix(".sha1").unwrap_or(url);
        let file_name = without_sha1.rsplit('/').next().unwrap_or_default();
        let version = file_name
            .strip_prefix("cleanroom-")
            .and_then(|value| value.strip_suffix(".zip"))
            .ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    format!("unexpected Cleanroom MMC URL '{url}'"),
                )
            })?;
        if without_sha1 != mmc_pack_release_url(version).unwrap() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("unexpected Cleanroom MMC URL '{url}'"),
            ));
        }
        let bytes = fixture_mmc_pack(version);
        if url.ends_with(".sha1") {
            Ok(format!("{:x}\n", Sha1::digest(&bytes)).into_bytes())
        } else {
            Ok(bytes)
        }
    }
}

pub fn fixture_mmc_pack(version: &str) -> Vec<u8> {
    fixture_mmc_pack_with_lwjgl(version, true)
}

pub fn fixture_mmc_pack_without_lwjgl(version: &str) -> Vec<u8> {
    fixture_mmc_pack_with_lwjgl(version, false)
}

fn fixture_mmc_pack_with_lwjgl(version: &str, include_lwjgl: bool) -> Vec<u8> {
    let cursor = Cursor::new(Vec::new());
    let mut zip = zip::ZipWriter::new(cursor);
    let options = SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
    let manifest = serde_json::json!({
        "formatVersion": 1,
        "components": [
            {"uid": "net.minecraft", "version": "1.12.2"},
            {"uid": "net.minecraftforge", "version": version}
        ]
    });
    let forge_patch = serde_json::json!({
        "libraries": [{"name": format!("com.cleanroommc:cleanroom:{version}")}]
    });
    let mut entries = vec![
        ("mmc-pack.json", serde_json::to_vec(&manifest).unwrap()),
        (
            "patches/net.minecraft.json",
            br#"{"libraries":[]}"#.to_vec(),
        ),
        (
            "patches/net.minecraftforge.json",
            serde_json::to_vec(&forge_patch).unwrap(),
        ),
    ];
    if include_lwjgl {
        entries.push(("patches/org.lwjgl3.json", br#"{"libraries":[]}"#.to_vec()));
    }
    for (name, bytes) in entries {
        zip.start_file(name, options).unwrap();
        zip.write_all(&bytes).unwrap();
    }
    zip.finish().unwrap().into_inner()
}

impl CurseForgeApi for FixtureApi {
    fn get_project(&self, project_id: u64) -> Result<CfProject, ResolveError> {
        Ok(match project_id {
            10 => CfProject::new_for_test(10, "Common Mod", "common-mod", 6, vec!["CommonDev"]),
            20 => CfProject::new_for_test(20, "Client Mod", "client-mod", 6, vec!["ClientDev"]),
            30 => CfProject::new_for_test(30, "Server Mod", "server-mod", 6, vec!["ServerDev"]),
            40 => {
                CfProject::new_for_test(40, "Resource Pack", "resource-pack", 12, vec!["PackDev"])
            }
            50 => {
                CfProject::new_for_test(50, "Shader Pack", "shader-pack", 6552, vec!["ShaderDev"])
            }
            60 => CfProject::new_for_test(
                60,
                "Unknown Side Mod",
                "unknown-side-mod",
                6,
                vec!["SideDev"],
            ),
            70 => CfProject::new_for_test(
                70,
                "Project Inferred Side Mod",
                "project-inferred-side-mod",
                6,
                vec!["SideDev"],
            ),
            1_005_815 => {
                CfProject::new_for_test(1_005_815, "Fugue", "fugue", 6, vec!["CleanroomMC"])
            }
            1_214_906 => CfProject::new_for_test(
                1_214_906,
                "Cleanroom Relauncher Unofficial",
                "cleanroom-relauncher-unofficial",
                6,
                vec!["CleanroomMC"],
            ),
            other => CfProject::new_for_test(
                other,
                format!("Project {other}"),
                format!("project-{other}"),
                6,
                vec!["Dev"],
            ),
        })
    }

    fn get_file(&self, project_id: u64, file_id: u64) -> Result<CfFile, ResolveError> {
        Ok(match project_id {
            10 => cf_file(
                file_id,
                project_id,
                "common-mod.jar",
                &["Forge", "1.12.2", "Client", "Server"],
            ),
            20 => cf_file(
                file_id,
                project_id,
                "client-mod.jar",
                &["Forge", "1.12.2", "Client"],
            ),
            30 => cf_file(
                file_id,
                project_id,
                "server-mod.jar",
                &["Forge", "1.12.2", "Server"],
            ),
            40 => cf_file(file_id, project_id, "resource-pack.zip", &["1.12.2"]),
            50 => cf_file(file_id, project_id, "shader-pack.zip", &["1.12.2"]),
            60 => cf_file(
                file_id,
                project_id,
                "unknown-side-mod.jar",
                &["Forge", "1.12.2"],
            ),
            70 => cf_file(
                file_id,
                project_id,
                "project-inferred-side-mod.jar",
                &["Forge", "1.12.2"],
            ),
            1_005_815 => cf_file(
                file_id,
                project_id,
                "fugue.jar",
                &["Forge", "1.12.2", "Client", "Server"],
            ),
            1_214_906 => cf_file(
                file_id,
                project_id,
                "cleanroom-relauncher.jar",
                &["Forge", "1.12.2", "Client"],
            ),
            _ => cf_file(
                file_id,
                project_id,
                "file.jar",
                &["Forge", "1.12.2", "Client", "Server"],
            ),
        })
    }

    fn find_project_by_slug(&self, slug: &str) -> Result<CfProject, ResolveError> {
        match slug {
            "fugue" => self.get_project(1_005_815),
            "cleanroom-relauncher-unofficial" => self.get_project(1_214_906),
            "common-mod" => self.get_project(10),
            "client-mod" => self.get_project(20),
            "server-mod" => self.get_project(30),
            "resource-pack" => self.get_project(40),
            "shader-pack" => self.get_project(50),
            other => Err(ResolveError::Api(format!("unknown slug '{other}'"))),
        }
    }

    fn list_files(
        &self,
        project_id: u64,
        _minecraft_version: &str,
        _loader: &str,
    ) -> Result<Vec<CfFile>, ResolveError> {
        if project_id == 70 {
            return Ok(vec![cf_file(
                7070,
                70,
                "project-inferred-side-mod-old.jar",
                &["Forge", "1.12.2"],
            )]);
        }
        Ok(vec![self.get_file(project_id, project_id + 1000)?])
    }

    fn list_project_files(&self, project_id: u64) -> Result<Vec<CfFile>, ResolveError> {
        if project_id == 70 {
            return Ok(vec![cf_file(
                7071,
                70,
                "project-inferred-side-mod-newer.jar",
                &["Forge", "1.20.1", "Client", "Server"],
            )]);
        }
        self.list_files(project_id, "", "")
    }
}

pub fn cf_file(id: u64, project_id: u64, file_name: &str, game_versions: &[&str]) -> CfFile {
    CfFile {
        id,
        mod_id: project_id,
        file_name: file_name.to_string(),
        display_name: Some(file_name.to_string()),
        is_available: true,
        file_status: 4,
        release_type: ReleaseType::Stable,
        game_versions: game_versions
            .iter()
            .map(|value| value.to_string())
            .collect(),
        dependencies: Vec::new(),
        download_url: Some(format!(
            "https://edge.forgecdn.net/files/{project_id}/{id}/{file_name}"
        )),
        hashes: vec![FileHash {
            value: "0123456789abcdef0123456789abcdef01234567".to_string(),
            algo: 1,
        }],
        file_length: 1234,
    }
}

pub fn write_build_project(project: &Path) {
    std::fs::write(
        project.join("config.toml"),
        toml::to_string_pretty(&sample_config()).unwrap(),
    )
    .unwrap();
    write_entries(
        project.join("common.json"),
        &[entry(10, 1010, DependencySide::Common)],
    );
    write_entries(
        project.join("client.json"),
        &[entry(20, 1020, DependencySide::Client)],
    );
    write_entries(
        project.join("server.json"),
        &[entry(30, 1030, DependencySide::Server)],
    );
    write_entries(
        project.join("resourcepacks.json"),
        &[
            entry(40, 1040, DependencySide::Resourcepack),
            DependencyEntry {
                dependency_type: DependencyType::ShaderPack,
                ..entry(50, 1050, DependencySide::Resourcepack)
            },
        ],
    );
    std::fs::create_dir_all(project.join("overrides/common/config")).unwrap();
    std::fs::create_dir_all(project.join("overrides/client")).unwrap();
    std::fs::create_dir_all(project.join("overrides/server")).unwrap();
    std::fs::write(
        project.join("overrides/common/config/common.json"),
        "{ \"x\": 1 }",
    )
    .unwrap();
    std::fs::write(project.join("overrides/client/options.txt"), "guiScale:2").unwrap();
    std::fs::write(
        project.join("overrides/server/server.properties"),
        "motd=mcmpu",
    )
    .unwrap();
    std::fs::write(project.join("README.md"), "# Example").unwrap();
    std::fs::write(project.join("LICENSE"), "MIT").unwrap();
}

pub fn entry(project_id: u64, file_id: u64, side: DependencySide) -> DependencyEntry {
    DependencyEntry {
        project_id: Some(project_id),
        file_id: Some(file_id),
        slug: Some(format!("project-{project_id}")),
        loader: Some(LoaderKind::Cleanroom.manifest_name().to_string()),
        side,
        dependency_type: DependencyType::Mod,
        name: format!("Project {project_id}"),
        authors: vec!["Dev".to_string()],
        required: true,
    }
}

pub fn write_entries(path: impl AsRef<Path>, entries: &[DependencyEntry]) {
    std::fs::write(path, serde_json::to_string_pretty(entries).unwrap()).unwrap();
}

pub fn write_empty_lists(project: &Path) {
    write_entries(project.join("common.json"), &[]);
    write_entries(project.join("client.json"), &[]);
    write_entries(project.join("server.json"), &[]);
    write_entries(project.join("resourcepacks.json"), &[]);
}
