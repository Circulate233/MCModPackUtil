use mcmpu::config::{Config, LoaderKind, TestRunConfig};
use mcmpu::curseforge::model::{CfFile, CfProject, FileDependency, ReleaseType, SideSupport};
use mcmpu::dependency::{DependencyEntry, DependencySide, DependencyType};
use mcmpu::manifest::curseforge_pack::ImportedManifest;
use mcmpu::manifest::generated::{generate_modlist_html, generate_pack_manifest};
use mcmpu::server::files::{ServerJavaConfig, ServerMemoryError, memory_mebibytes};
use mcmpu::server::loader::{LoaderOutput, plan_server_install_loader};
use mcmpu::zip::optimize::minify_json_bytes;
use std::collections::BTreeSet;

mod support;

#[test]
fn test_run_config_is_stored_separately_from_build_config() {
    let mut config = support::sample_config();
    config.test_run.player_name = "LocalPlayer".to_string();
    let serialized = toml::to_string_pretty(&config).unwrap();
    let round_trip: Config = toml::from_str(&serialized).unwrap();

    assert!(!serialized.contains("testRun"));
    assert_eq!(round_trip.test_run, TestRunConfig::default());
}

#[test]
fn main_config_rejects_legacy_test_run_section() {
    let serialized = toml::to_string_pretty(&support::sample_config()).unwrap();
    let invalid = format!("{serialized}\n[testRun]\nplayer_name = \"Legacy\"\n");

    let error = toml::from_str::<Config>(&invalid).unwrap_err();

    assert!(error.to_string().contains("unknown field `testRun`"));
}

#[test]
fn strict_config_loading_rejects_a_missing_project_java_major_version() {
    let temp = tempfile::tempdir().unwrap();
    let mut value = toml::Value::try_from(support::sample_config()).unwrap();
    value
        .get_mut("java")
        .and_then(toml::Value::as_table_mut)
        .unwrap()
        .remove("version_major");
    std::fs::write(
        temp.path().join("config.toml"),
        toml::to_string_pretty(&value).unwrap(),
    )
    .unwrap();

    let error = Config::from_path(temp.path().join("config.toml")).unwrap_err();

    assert!(matches!(
        error,
        mcmpu::config::ConfigError::MissingJavaVersion
    ));
}

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

    let config = Config::from_imported_manifest(&manifest, "zh-CN".to_string()).unwrap();

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
    assert_eq!(
        DependencyType::Mod.curseforge_file_page_url("onlyoneitem", 8_412_719),
        "https://www.curseforge.com/minecraft/mc-mods/onlyoneitem/files/8412719"
    );
    assert_eq!(
        DependencyType::ResourcePack.curseforge_file_page_url("faithful", 123),
        "https://www.curseforge.com/minecraft/texture-packs/faithful/files/123"
    );
    assert_eq!(
        DependencyType::ShaderPack.curseforge_file_page_url("complementary", 456),
        "https://www.curseforge.com/minecraft/shaders/complementary/files/456"
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
    let mut config = support::sample_config();
    config.pack.loader_version = "0.6.2-alpha".to_string();
    config.pack.forge_compat_version = Some("14.23.5.2860".to_string());
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
    assert_eq!(
        manifest["minecraft"]["modLoaders"][0]["id"],
        "forge-14.23.5.2860"
    );
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

#[test]
fn server_memory_converts_exact_java_units_and_splits_jvm_arguments() {
    let memory = mcmpu::config::JavaMemoryConfig {
        min_memory: "1048576K".to_string(),
        max_memory: "4g".to_string(),
        extra_args: vec!["-XX:+UseG1GC -Dmode=server".to_string()],
    };

    let java = ServerJavaConfig::from_memory_config(17, &memory).unwrap();

    assert_eq!(java.major, 17);
    assert_eq!(java.min_memory_mb, 1024);
    assert_eq!(java.max_memory_mb, 4096);
    assert_eq!(java.jvm_args, vec!["-XX:+UseG1GC", "-Dmode=server"]);
    assert_eq!(java.server_args, vec!["nogui"]);
}

#[test]
fn server_memory_rejects_fractional_zero_overflow_and_inverted_values() {
    assert!(matches!(
        memory_mebibytes("memory", "1K"),
        Err(ServerMemoryError::FractionalMebibyte { .. })
    ));
    assert!(matches!(
        memory_mebibytes("memory", "0M"),
        Err(ServerMemoryError::InvalidFormat { .. })
    ));
    assert!(matches!(
        memory_mebibytes("memory", "4194304G"),
        Err(ServerMemoryError::Overflow { .. })
    ));

    let memory = mcmpu::config::JavaMemoryConfig {
        min_memory: "2G".to_string(),
        max_memory: "1024M".to_string(),
        extra_args: Vec::new(),
    };
    assert!(matches!(
        ServerJavaConfig::from_memory_config(8, &memory),
        Err(ServerMemoryError::InvertedRange { .. })
    ));
}

#[test]
fn server_install_loader_plans_cover_all_outputs() {
    let fabric = plan_server_install_loader(&LoaderKind::Fabric, "1.20.1", "0.16.14").unwrap();
    assert_eq!(
        fabric.output,
        LoaderOutput::ExactJar {
            path: "fabric-server-launch.jar".to_string(),
            main_class: Some(
                "net.fabricmc.loader.impl.launch.server.FabricServerLauncher".to_string()
            ),
        }
    );
    assert_eq!(
        fabric.installer.sha1_sidecar,
        format!("{}.sha1", fabric.installer.url)
    );

    let neoforge = plan_server_install_loader(&LoaderKind::NeoForge, "1.21.1", "21.1.190").unwrap();
    assert_eq!(
        neoforge.output,
        LoaderOutput::ModernArgs {
            windows: "libraries/net/neoforged/neoforge/21.1.190/win_args.txt".to_string(),
            unix: "libraries/net/neoforged/neoforge/21.1.190/unix_args.txt".to_string(),
        }
    );

    let cleanroom =
        plan_server_install_loader(&LoaderKind::Cleanroom, "1.12.2", "0.3.16-alpha").unwrap();
    assert_eq!(
        cleanroom.output,
        LoaderOutput::ExactJar {
            path: "cleanroom-0.3.16-alpha.jar".to_string(),
            main_class: None,
        }
    );

    let legacy_forge = plan_server_install_loader(&LoaderKind::Forge, "1.16.5", "36.2.42").unwrap();
    assert_eq!(
        legacy_forge.output,
        LoaderOutput::ExactJar {
            path: "forge-1.16.5-36.2.42.jar".to_string(),
            main_class: None,
        }
    );

    let modern_forge = plan_server_install_loader(&LoaderKind::Forge, "1.17", "37.1.1").unwrap();
    assert_eq!(
        modern_forge.output,
        LoaderOutput::ModernArgs {
            windows: "libraries/net/minecraftforge/forge/1.17-37.1.1/win_args.txt".to_string(),
            unix: "libraries/net/minecraftforge/forge/1.17-37.1.1/unix_args.txt".to_string(),
        }
    );
}

#[test]
fn server_install_forge_rejects_non_numeric_minecraft_versions() {
    let error = plan_server_install_loader(&LoaderKind::Forge, "1.20-pre1", "47.0.0").unwrap_err();
    assert!(error.to_string().contains("numeric components"));
}
