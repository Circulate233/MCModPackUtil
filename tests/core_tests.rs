use std::collections::BTreeSet;

use mcmpu::config::{Config, LoaderKind};
use mcmpu::curseforge::model::{CfFile, CfProject, FileDependency, ReleaseType, SideSupport};
use mcmpu::dependency::{DependencyEntry, DependencySide, DependencyType};
use mcmpu::manifest::curseforge_pack::ImportedManifest;
use mcmpu::manifest::generated::{generate_modlist_html, generate_pack_manifest};
use mcmpu::server::files::{ServerFile, ServerFilesManifest, ServerJavaConfig};
use mcmpu::server::loader::{LoaderInstallerPlan, plan_loader_install};
use mcmpu::server::scripts::{render_start_bat, render_start_sh};
use mcmpu::zip::optimize::minify_json_bytes;

mod support;

#[test]
fn rejects_placeholder_manifest_version() {
    let raw = r#"{
      "manifestType": "minecraftModpack",
      "manifestVersion": 1,
      "name": "ExamplePack-cleanroom",
      "version": "${version}",
      "author": "ExampleAuthor,SecondAuthor",
      "overrides": "overrides",
      "minecraft": {
        "version": "1.12.2",
        "modLoaders": [{"id": "forge-1.0.0", "primary": true}]
      },
      "files": []
    }"#;

    let err = ImportedManifest::from_slice(raw.as_bytes()).unwrap_err();

    assert!(err.to_string().contains("manifest.version"));
    assert!(err.to_string().contains("${version}"));
}

#[test]
fn cleanroom_pack_name_generates_cleanroom_config() {
    let raw = r#"{
      "manifestType": "minecraftModpack",
      "manifestVersion": 1,
      "name": "ExamplePack-cleanroom",
      "version": "1.20.0",
      "author": "ExampleAuthor,SecondAuthor",
      "overrides": "overrides",
      "minecraft": {
        "version": "1.12.2",
        "modLoaders": [{"id": "forge-1.0.0", "primary": true}]
      },
      "files": []
    }"#;
    let manifest = ImportedManifest::from_slice(raw.as_bytes()).unwrap();

    let config = Config::from_imported_manifest(&manifest, "zh-CN".to_string());

    assert_eq!(config.pack.loader, LoaderKind::Cleanroom);
    assert_eq!(config.pack.loader_version, "1.0.0");
    assert_eq!(config.pack.forge_compat_version.as_deref(), Some("1.0.0"));
    assert_eq!(config.pack.authors, vec!["ExampleAuthor", "SecondAuthor"]);
    assert_eq!(config.cleanroom.as_ref().unwrap().scalar_version, 2);
}

#[test]
fn config_rejects_legacy_api_key_and_proxy_fields() {
    let temp = tempfile::tempdir().unwrap();
    let mut config = toml::to_string_pretty(&support::sample_config()).unwrap();
    config = config.replace("[network]", "api_key = \"legacy\"\n\n[network]");
    std::fs::write(temp.path().join("config.toml"), &config).unwrap();

    let api_key_error = Config::from_path(temp.path().join("config.toml")).unwrap_err();

    assert!(api_key_error.to_string().contains("api_key"));

    let mut config = toml::to_string_pretty(&support::sample_config()).unwrap();
    config = config.replace("proxy_ip = \"\"", "proxy = \"\"");
    std::fs::write(temp.path().join("config.toml"), &config).unwrap();

    let proxy_error = Config::from_path(temp.path().join("config.toml")).unwrap_err();

    assert!(proxy_error.to_string().contains("proxy"));
}

#[test]
fn config_rejects_stored_curseforge_api_key_fields() {
    let temp = tempfile::tempdir().unwrap();
    let valid = toml::to_string_pretty(&support::sample_config()).unwrap();

    std::fs::write(
        temp.path().join("config.toml"),
        format!("{valid}\n[curseforge]\ncurseforge_api_key = \"legacy\"\n"),
    )
    .unwrap();

    let section_error = Config::from_path(temp.path().join("config.toml")).unwrap_err();
    let section_message = section_error.to_string();
    assert!(section_message.contains("[curseforge]"));
    assert!(section_message.contains("--curseforge-api-key"));
    assert!(section_message.contains("MCMODPACKUTIL_CURSEFORGE_API_KEY"));
    assert!(section_message.contains("official build"));

    std::fs::write(
        temp.path().join("config.toml"),
        valid.replace("[network]", "curseforge_api_key = \"legacy\"\n\n[network]"),
    )
    .unwrap();

    let field_error = Config::from_path(temp.path().join("config.toml")).unwrap_err();
    let field_message = field_error.to_string();
    assert!(field_message.contains("curseforge_api_key"));
    assert!(field_message.contains("--curseforge-api-key"));
    assert!(field_message.contains("MCMODPACKUTIL_CURSEFORGE_API_KEY"));
    assert!(field_message.contains("official build"));
}

#[test]
fn dependency_entry_round_trips_without_version_field() {
    let raw = r#"[{
      "project_id": 521480,
      "file_id": 5811529,
      "slug": "skin-layers-3d",
      "loader": "forge",
      "type": "mod",
      "name": "Skin Layers 3D",
      "required": true
    }]"#;

    let entries: Vec<DependencyEntry> = serde_json::from_str(raw).unwrap();

    assert_eq!(entries[0].project_id, Some(521480));
    assert_eq!(entries[0].file_id, Some(5811529));
    assert_eq!(entries[0].side, DependencySide::Common);
    assert_eq!(entries[0].dependency_type, DependencyType::Mod);
    let serialized = serde_json::to_value(&entries[0]).unwrap();
    let object = serialized.as_object().unwrap();
    assert!(!object.contains_key("version"));
    assert!(!object.contains_key("side"));
    assert!(!object.contains_key("authors"));
}

#[test]
fn cf_file_classifies_side_loader_and_type() {
    let file = CfFile {
        id: 6147102,
        mod_id: 873867,
        file_name: "!Red-Core-MC-1.8-1.12-0.6.jar".to_string(),
        display_name: Some("Red Core 0.6".to_string()),
        file_length: 42,
        is_available: true,
        file_status: 4,
        release_type: ReleaseType::Beta,
        game_versions: BTreeSet::from([
            "1.12.2".to_string(),
            "Forge".to_string(),
            "Client".to_string(),
            "Server".to_string(),
        ]),
        dependencies: vec![],
        download_url: Some("https://edge.forgecdn.net/files/6147/102/file.jar".to_string()),
        hashes: vec![],
    };
    let project = CfProject::new_for_test(873867, "Red Core", "red-core", 6, vec!["Desoroxxx"]);

    let entry = DependencyEntry::from_curseforge(&project, &file).unwrap();

    assert_eq!(entry.loader.as_deref(), Some("forge"));
    assert_eq!(entry.side, DependencySide::Common);
    assert_eq!(entry.dependency_type, DependencyType::Mod);
}

#[test]
fn cf_file_side_falls_back_to_project_side_metadata() {
    let file = CfFile {
        id: 1060,
        mod_id: 60,
        file_name: "project-side-only.jar".to_string(),
        display_name: Some("Project Side Only".to_string()),
        file_length: 42,
        is_available: true,
        file_status: 4,
        release_type: ReleaseType::Stable,
        game_versions: BTreeSet::from(["Forge".to_string(), "1.12.2".to_string()]),
        dependencies: vec![],
        download_url: Some(
            "https://edge.forgecdn.net/files/1/60/project-side-only.jar".to_string(),
        ),
        hashes: vec![],
    };
    let project = CfProject::new_for_test(
        60,
        "Project Side Only",
        "project-side-only",
        6,
        vec!["SideDev"],
    )
    .with_side_support(Some(SideSupport::Required), Some(SideSupport::Required));

    let entry = DependencyEntry::from_curseforge(&project, &file).unwrap();

    assert_eq!(entry.side, DependencySide::Common);
}

#[test]
fn cf_file_side_tags_take_precedence_over_project_side_metadata() {
    let file = CfFile {
        id: 1061,
        mod_id: 61,
        file_name: "client-file.jar".to_string(),
        display_name: Some("Client File".to_string()),
        file_length: 42,
        is_available: true,
        file_status: 4,
        release_type: ReleaseType::Stable,
        game_versions: BTreeSet::from([
            "Forge".to_string(),
            "1.12.2".to_string(),
            "Client".to_string(),
        ]),
        dependencies: vec![],
        download_url: Some("https://edge.forgecdn.net/files/1/61/client-file.jar".to_string()),
        hashes: vec![],
    };
    let project = CfProject::new_for_test(61, "Client File", "client-file", 6, vec!["SideDev"])
        .with_side_support(None, Some(SideSupport::Required));

    let entry = DependencyEntry::from_curseforge(&project, &file).unwrap();

    assert_eq!(entry.side, DependencySide::Client);
}

#[test]
fn cf_project_type_maps_resource_and_shader_packs() {
    let resource_project = CfProject::new_for_test(100, "Pack", "pack", 12, vec!["A"]);
    let shader_project = CfProject::new_for_test(101, "Shader", "shader", 6552, vec!["B"]);

    assert_eq!(
        DependencyType::from_curseforge_class(resource_project.class_id).unwrap(),
        DependencyType::ResourcePack
    );
    assert_eq!(
        DependencyType::from_curseforge_class(shader_project.class_id).unwrap(),
        DependencyType::ShaderPack
    );
}

#[test]
fn generated_modlist_escapes_project_names_and_authors() {
    let entries = vec![DependencyEntry {
        project_id: Some(123),
        file_id: Some(456),
        slug: Some("test".to_string()),
        loader: Some("forge".to_string()),
        side: DependencySide::Common,
        dependency_type: DependencyType::Mod,
        name: "A <B>".to_string(),
        authors: vec!["C & D".to_string()],
        required: true,
    }];

    let html = generate_modlist_html(&entries);

    assert_eq!(
        html,
        "<ul><li><a href=\"https://www.curseforge.com/projects/123\">A &lt;B&gt; (by C &amp; D)</a></li></ul>"
    );
}

#[test]
fn generated_client_manifest_uses_locked_file_coordinates() {
    let config = support::sample_config();
    let entries = vec![DependencyEntry {
        project_id: Some(873867),
        file_id: Some(6147102),
        slug: Some("red-core".to_string()),
        loader: Some("forge".to_string()),
        side: DependencySide::Common,
        dependency_type: DependencyType::Mod,
        name: "Red Core".to_string(),
        authors: vec!["Desoroxxx".to_string()],
        required: true,
    }];

    let manifest = generate_pack_manifest(&config, &entries, "overrides").unwrap();

    assert_eq!(manifest["manifestType"], "minecraftModpack");
    assert_eq!(manifest["files"][0]["projectID"], 873867);
    assert_eq!(manifest["files"][0]["fileID"], 6147102);
    assert_eq!(manifest["minecraft"]["version"], "1.12.2");
}

#[test]
fn minify_json_preserves_string_content() {
    let input = br#"{ "text": "hello   world", "nested": [1, 2, 3] }"#;

    let minified = minify_json_bytes(input).unwrap();

    assert_eq!(
        std::str::from_utf8(&minified).unwrap(),
        r#"{"text":"hello   world","nested":[1,2,3]}"#
    );
}

#[test]
fn minify_json_keeps_empty_placeholder_files() {
    assert_eq!(minify_json_bytes(b"").unwrap(), b"");
    assert_eq!(minify_json_bytes(b"  \r\n\t").unwrap(), b"  \r\n\t");
}

#[test]
fn server_scripts_include_download_verification_without_eula_prompt() {
    let manifest = ServerFilesManifest {
        language: "en-US".to_string(),
        curseforge_api_key: "test-key".to_string(),
        java: sample_server_java(),
        files: vec![ServerFile {
            path: "mods/example.jar".to_string(),
            url: "https://edge.forgecdn.net/files/1/2/example.jar".to_string(),
            sha1: "0123456789abcdef0123456789abcdef01234567".to_string(),
            size: 12,
        }],
        loader: LoaderInstallerPlan::Forge {
            installer_url: "https://maven.minecraftforge.net/forge-installer.jar".to_string(),
            installer_sha1: None,
        },
    };

    let bat = render_start_bat(&manifest);
    let sh = render_start_sh(&manifest);

    assert!(mcmpu::server::scripts::render_download_ps1().contains("Get-FileHash"));
    assert!(!bat.contains("eula=true"));
    assert!(!bat.contains("set /p ACCEPT"));
    assert!(bat.contains("SERVER_JAR"));
    assert!(bat.contains("call :download_file"));
    assert!(bat.contains("pause"));
    assert!(bat.contains("setlocal DisableDelayedExpansion"));
    assert!(!bat.contains("enabledelayedexpansion"));
    assert!(sh.contains("sha1sum"));
    assert!(!sh.contains("eula=true"));
    assert!(!sh.contains("read ACCEPT"));
    assert!(sh.contains("fabric-server-launch.jar"));
}

#[test]
fn server_scripts_leave_eula_to_minecraft_server() {
    let manifest = ServerFilesManifest {
        language: "zh-CN".to_string(),
        curseforge_api_key: "test-key".to_string(),
        java: sample_server_java(),
        files: Vec::new(),
        loader: LoaderInstallerPlan::Forge {
            installer_url: "https://maven.minecraftforge.net/forge-installer.jar".to_string(),
            installer_sha1: None,
        },
    };

    let sh = render_start_sh(&manifest);
    let bat = render_start_bat(&manifest);

    assert!(!bat.contains("MinecraftEULA"));
    assert!(!bat.contains("Type yes"));
    assert!(!sh.contains("Please read https://aka.ms/MinecraftEULA"));
    assert!(!sh.contains("Type yes to accept the Minecraft EULA"));
    assert!(!sh.contains("请阅读"));
    assert!(!sh.contains("输入 yes"));
}

#[test]
fn server_scripts_require_java_placeholder_and_use_memory_extra_args() {
    let manifest = ServerFilesManifest {
        language: "en-US".to_string(),
        curseforge_api_key: "test-key".to_string(),
        java: ServerJavaConfig {
            min_memory: "4G".to_string(),
            max_memory: "8G".to_string(),
            extra_args: vec!["-XX:+UseG1GC".to_string(), "-Dfoo=bar".to_string()],
        },
        files: Vec::new(),
        loader: LoaderInstallerPlan::Forge {
            installer_url: "https://maven.minecraftforge.net/forge-installer.jar".to_string(),
            installer_sha1: None,
        },
    };

    let bat = render_start_bat(&manifest);
    let sh = render_start_sh(&manifest);

    assert!(bat.contains("set \"JAVA_EXE=CHANGE_ME_JAVA_PATH\""));
    assert!(bat.contains("\"%JAVA_EXE%\" -Xms4G -Xmx8G -XX:+UseG1GC -Dfoo=bar -jar"));
    assert!(sh.contains("JAVA_EXE=\"CHANGE_ME_JAVA_PATH\""));
    assert!(sh.contains("\"$JAVA_EXE\" -Xms4G -Xmx8G -XX:+UseG1GC -Dfoo=bar -jar"));
}

#[test]
fn server_bat_pauses_on_failures_when_double_clicked() {
    let manifest = ServerFilesManifest {
        language: "en-US".to_string(),
        curseforge_api_key: "test-key".to_string(),
        java: ServerJavaConfig {
            min_memory: "4G".to_string(),
            max_memory: "8G".to_string(),
            extra_args: Vec::new(),
        },
        files: Vec::new(),
        loader: LoaderInstallerPlan::Forge {
            installer_url: "https://maven.minecraftforge.net/forge-installer.jar".to_string(),
            installer_sha1: None,
        },
    };

    let bat = render_start_bat(&manifest);
    let sh = render_start_sh(&manifest);

    assert!(bat.contains(":fail"));
    assert!(bat.contains("pause"));
    assert!(bat.contains("exit /b 1"));
    assert!(bat.contains("JAVA path is not configured"));
    assert!(bat.contains("CHANGE_ME_JAVA_PATH"));
    assert!(sh.contains("JAVA path is not configured"));
    assert!(sh.contains("CHANGE_ME_JAVA_PATH"));
}

#[test]
fn server_download_helper_uses_api_key_header_and_sha1_verification() {
    let manifest = ServerFilesManifest {
        language: "en-US".to_string(),
        curseforge_api_key: "test-key".to_string(),
        java: sample_server_java(),
        files: vec![ServerFile {
            path: "mods/!red-core-873867-6147102.jar".to_string(),
            url: "https://edge.forgecdn.net/files/6147/102/!Red-Core.jar".to_string(),
            sha1: "0123456789abcdef0123456789abcdef01234567".to_string(),
            size: 12,
        }],
        loader: LoaderInstallerPlan::Forge {
            installer_url: "https://maven.minecraftforge.net/forge-installer.jar".to_string(),
            installer_sha1: None,
        },
    };

    let bat = render_start_bat(&manifest);
    let sh = render_start_sh(&manifest);
    let helper = mcmpu::server::scripts::render_download_ps1();

    assert!(bat.contains("set \"CF_API_KEY=test-key\""));
    assert!(bat.contains("call :download_file \"mods/!red-core-873867-6147102.jar\""));
    assert!(bat.contains("-File \"%~dp0mcmpu-download.ps1\""));
    assert!(helper.contains("Invoke-WebRequest"));
    assert!(helper.contains("\"x-api-key\" = $ApiKey"));
    assert!(helper.contains("Get-FileHash"));
    assert!(helper.contains("New-Item -ItemType Directory"));
    assert!(sh.contains("CF_API_KEY='test-key'"));
    assert!(sh.contains("curl -L -H \"x-api-key: $CF_API_KEY\""));
    assert!(sh.contains("wget --header=\"x-api-key: $CF_API_KEY\""));
}

#[test]
fn loader_install_plans_cover_supported_loaders() {
    assert!(matches!(
        plan_loader_install(&LoaderKind::Forge, "1.20.1", "47.4.0").unwrap(),
        LoaderInstallerPlan::Forge { .. }
    ));
    assert!(matches!(
        plan_loader_install(&LoaderKind::Fabric, "1.20.1", "0.16.14").unwrap(),
        LoaderInstallerPlan::Fabric { .. }
    ));
    assert!(matches!(
        plan_loader_install(&LoaderKind::NeoForge, "1.21.1", "21.1.190").unwrap(),
        LoaderInstallerPlan::NeoForge { .. }
    ));
    assert!(matches!(
        plan_loader_install(&LoaderKind::Cleanroom, "1.12.2", "0.3.0-alpha").unwrap(),
        LoaderInstallerPlan::Cleanroom { .. }
    ));
}

#[test]
fn required_dependency_report_detects_missing_projects() {
    let file = CfFile {
        id: 7977714,
        mod_id: 1519385,
        file_name: "circulation_networks-1.12.2-1.0.0.jar".to_string(),
        display_name: None,
        file_length: 42,
        is_available: true,
        file_status: 4,
        release_type: ReleaseType::Stable,
        game_versions: BTreeSet::from(["Forge".to_string(), "1.12.2".to_string()]),
        dependencies: vec![FileDependency {
            mod_id: 419286,
            relation_type: 3,
        }],
        download_url: Some("https://edge.forgecdn.net/files/7977/714/file.jar".to_string()),
        hashes: vec![],
    };
    let existing = BTreeSet::from([1519385]);

    let missing = file.missing_required_dependencies(&existing);

    assert_eq!(missing, vec![419286]);
}

fn sample_server_java() -> ServerJavaConfig {
    ServerJavaConfig {
        min_memory: "2G".to_string(),
        max_memory: "4G".to_string(),
        extra_args: Vec::new(),
    }
}
