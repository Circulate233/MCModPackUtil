use mcmpu::config::{DownloadSource, LoaderKind, TestRunConfig};
use mcmpu::curseforge::model::{CfFile, FileHash, ReleaseType};
use mcmpu::curseforge::resolver::{CurseForgeApi, ResolveError};
use mcmpu::dependency::DependencyType;
use mcmpu::net::{TransferEvent, TransferObserver, TransferPhase};
use mcmpu::project::jvm_args::{java_memory_arguments, split_jvm_argument_entries};
use mcmpu::project::launcher::{LaunchDescriptor, LaunchSide};
use mcmpu::project::run::{
    DownloadBytes, HttpDownloader, JavaLocator, LaunchCommand, NeverConsole, NeverStop,
    PendingSyncChange, PendingSyncEntry, PendingSyncManifest, ProcessError, ProcessOutputStream,
    ProcessRunner, ProcessSpec, RunDownloadRequest, RunError, RunEvent, RunFileReadySource,
    RunProgress, RunSide, RunSourceKind, RunSourceManifest, RunTermination, RunningProcess,
    SharedConsoleInput, StopSignal, SyncConfig, SyncTarget, SystemProcessRunner,
    apply_overlay_layers, apply_pending_sync, curseforge_cdn_artifact_url,
    finalize_pending_sync_after_run, offline_uuid_for_player, ordered_download_sources,
    plan_launch_command, plan_launch_command_from_descriptor, prepare_run_directory,
    prepare_run_directory_with_api, prepare_run_directory_with_api_and_progress,
    resolve_java_executable, resolve_or_download_java_executable, run_file_name,
    run_launch_command_with_stop_signal, scan_pending_sync, supervise_run,
    sync_config_for_project_for_gui, verify_or_download_run_files,
    verify_or_download_run_files_with_progress, write_sync_config_for_project,
};
use sha2::{Digest as Sha2Digest, Sha256};

mod support;

#[test]
fn test_run_config_defaults_and_round_trips() {
    let config = TestRunConfig::default();

    assert_eq!(config.player_name, "Player");
    assert_eq!(config.download_source, DownloadSource::Auto);
    assert_eq!(config.sync_interval_seconds, 30);
    assert!(config.java_path.is_empty());
    assert!(config.debug_extra_jvm_args.is_empty());

    let text = toml::to_string_pretty(&config).unwrap();
    let value: toml::Value = toml::from_str(&text).unwrap();
    assert!(value.get("java_version_major").is_none());
    let parsed: TestRunConfig = toml::from_str(&text).unwrap();

    assert_eq!(parsed, config);
}

#[test]
fn test_run_config_rejects_invalid_download_source() {
    let err = toml::from_str::<TestRunConfig>(
        r#"
player_name = "Player"
download_source = "random"
sync_interval_seconds = 30
debug_extra_jvm_args = []
java_path = ""
"#,
    )
    .unwrap_err();

    assert!(err.to_string().contains("download_source"));
}

#[test]
fn gui_test_run_config_loads_valid_fields_and_defaults_invalid_or_missing_fields() {
    let project = tempfile::tempdir().unwrap();
    std::fs::write(
        TestRunConfig::path(project.path()),
        r#"
player_name = "Developer"
download_source = "unsupported"
sync_interval_seconds = "invalid"
debug_extra_jvm_args = ["-Dfirst=true", 2]
unknown_field = true
"#,
    )
    .unwrap();

    let loaded = TestRunConfig::from_project_for_gui(project.path()).unwrap();

    assert_eq!(loaded.player_name, "Developer");
    assert_eq!(loaded.download_source, DownloadSource::Auto);
    assert_eq!(loaded.sync_interval_seconds, 30);
    assert!(loaded.debug_extra_jvm_args.is_empty());
    assert!(loaded.java_path.is_empty());
    assert!(TestRunConfig::from_project(project.path()).is_err());
}

#[test]
fn gui_test_run_config_uses_defaults_for_malformed_toml_while_strict_load_still_fails() {
    let project = tempfile::tempdir().unwrap();
    std::fs::write(TestRunConfig::path(project.path()), "player_name = [").unwrap();

    assert_eq!(
        TestRunConfig::from_project_for_gui(project.path()).unwrap(),
        TestRunConfig::default()
    );
    assert!(TestRunConfig::from_project(project.path()).is_err());
}

#[test]
fn configured_jvm_argument_fragments_become_individual_process_arguments() {
    let java = mcmpu::config::JavaMemoryConfig {
        min_memory: "8G".to_string(),
        max_memory: "8G".to_string(),
        extra_args: vec![
            "-XX:+UseZGC -XX:+UseCompactObjectHeaders -XX:ReservedCodeCacheSize=500M -XX:-DontCompileHugeMethods"
                .to_string(),
        ],
    };

    assert_eq!(
        java_memory_arguments(&java).unwrap(),
        [
            "-Xms8G",
            "-Xmx8G",
            "-XX:+UseZGC",
            "-XX:+UseCompactObjectHeaders",
            "-XX:ReservedCodeCacheSize=500M",
            "-XX:-DontCompileHugeMethods",
        ]
    );
    assert_eq!(
        split_jvm_argument_entries(&["-Ddebug=true  -XX:+UnlockDiagnosticVMOptions".to_string()]),
        ["-Ddebug=true", "-XX:+UnlockDiagnosticVMOptions"]
    );
}

#[test]
fn configured_jvm_argument_fragments_reject_memory_overrides_after_splitting() {
    let java = mcmpu::config::JavaMemoryConfig {
        min_memory: "4G".to_string(),
        max_memory: "8G".to_string(),
        extra_args: vec!["-Dmode=dev -Xmx12G".to_string()],
    };

    let error = java_memory_arguments(&java).unwrap_err();

    assert_eq!(
        error,
        mcmpu::project::jvm_args::JvmArgumentError::MemoryOverride {
            argument: "-Xmx12G".to_string(),
        }
    );
}

#[test]
fn run_launch_plan_normalizes_java_and_debug_argument_fragments() {
    let project = tempfile::tempdir().unwrap();
    let mut config = support::sample_config();
    config.java.client.min_memory = "8G".to_string();
    config.java.client.max_memory = "8G".to_string();
    config.java.client.extra_args = vec![
        "-XX:+UseZGC -XX:+UseCompactObjectHeaders -XX:ReservedCodeCacheSize=500M -XX:-DontCompileHugeMethods"
            .to_string(),
    ];
    config.test_run.debug_extra_jvm_args =
        vec!["-Ddebug=true -XX:+UnlockDiagnosticVMOptions".to_string()];
    let resolved = mcmpu::project::build::RuntimeResolvedLists {
        common: Vec::new(),
        client: Vec::new(),
        server: Vec::new(),
        resourcepacks: Vec::new(),
    };

    let plan = prepare_run_directory(project.path(), &config, &resolved, RunSide::Client).unwrap();

    assert_eq!(
        plan.java_args,
        [
            "-Xms8G",
            "-Xmx8G",
            "-XX:+UseZGC",
            "-XX:+UseCompactObjectHeaders",
            "-XX:ReservedCodeCacheSize=500M",
            "-XX:-DontCompileHugeMethods",
            "-Ddebug=true",
            "-XX:+UnlockDiagnosticVMOptions",
        ]
    );
}

#[test]
fn run_launch_plan_rejects_debug_memory_overrides_after_splitting() {
    let project = tempfile::tempdir().unwrap();
    let mut config = support::sample_config();
    config.test_run.debug_extra_jvm_args = vec!["-Ddebug=true -Xms1G".to_string()];
    let resolved = mcmpu::project::build::RuntimeResolvedLists {
        common: Vec::new(),
        client: Vec::new(),
        server: Vec::new(),
        resourcepacks: Vec::new(),
    };

    let error =
        prepare_run_directory(project.path(), &config, &resolved, RunSide::Client).unwrap_err();

    assert!(matches!(
        error,
        RunError::JvmArguments(
            mcmpu::project::jvm_args::JvmArgumentError::MemoryOverride { ref argument }
        ) if argument == "-Xms1G"
    ));
}

#[test]
fn sync_config_rejects_paths_outside_run_root_and_generated_directories() {
    let project = tempfile::tempdir().unwrap();
    for path in ["../outside", "mods", "assets/cache"] {
        let config = SyncConfig {
            include_paths: vec![path.to_string()],
        };
        assert!(matches!(
            write_sync_config_for_project(project.path(), RunSide::Client, &config),
            Err(RunError::InvalidSyncIncludePath { .. })
        ));
    }
}

#[test]
fn client_and_server_sync_configs_are_written_independently() {
    let project = tempfile::tempdir().unwrap();
    let client = SyncConfig {
        include_paths: vec!["config".to_string(), "client-options".to_string()],
    };
    let server = SyncConfig {
        include_paths: vec!["config".to_string(), "server-properties".to_string()],
    };

    write_sync_config_for_project(project.path(), RunSide::Client, &client).unwrap();
    write_sync_config_for_project(project.path(), RunSide::Server, &server).unwrap();

    assert_eq!(
        SyncConfig::from_path(project.path().join("run/client/.mcmpu-sync.toml")).unwrap(),
        client
    );
    assert_eq!(
        SyncConfig::from_path(project.path().join("run/server/.mcmpu-sync.toml")).unwrap(),
        server
    );
}

#[test]
fn gui_sync_config_retains_valid_paths_and_ignores_invalid_entries_and_unknown_fields() {
    let project = tempfile::tempdir().unwrap();
    let run_dir = project.path().join("run/client");
    std::fs::create_dir_all(&run_dir).unwrap();
    std::fs::write(
        run_dir.join(".mcmpu-sync.toml"),
        r#"
include_paths = ["config", "../outside", "mods", 7, "options"]
unknown_field = true
"#,
    )
    .unwrap();

    let loaded = sync_config_for_project_for_gui(project.path(), RunSide::Client).unwrap();

    assert_eq!(loaded.include_paths, ["config", "options"]);
    assert!(SyncConfig::from_path(run_dir.join(".mcmpu-sync.toml")).is_err());
}

#[test]
fn gui_sync_config_uses_project_defaults_for_malformed_toml() {
    let project = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(project.path().join("overrides/common/config")).unwrap();
    std::fs::create_dir_all(project.path().join("overrides/client/options")).unwrap();
    let run_dir = project.path().join("run/client");
    std::fs::create_dir_all(&run_dir).unwrap();
    std::fs::write(run_dir.join(".mcmpu-sync.toml"), "include_paths = [").unwrap();

    let loaded = sync_config_for_project_for_gui(project.path(), RunSide::Client).unwrap();

    assert_eq!(loaded.include_paths, ["config", "options"]);
    assert!(SyncConfig::from_path(run_dir.join(".mcmpu-sync.toml")).is_err());
}

#[test]
fn gui_test_run_config_propagates_non_content_io_errors() {
    let project = tempfile::tempdir().unwrap();
    std::fs::create_dir(TestRunConfig::path(project.path())).unwrap();

    let error = TestRunConfig::from_project_for_gui(project.path()).unwrap_err();

    assert!(matches!(error, mcmpu::config::ConfigError::Read { .. }));
}

#[test]
fn gui_sync_config_propagates_config_and_default_path_io_errors() {
    let config_project = tempfile::tempdir().unwrap();
    let run_dir = config_project.path().join("run/client");
    std::fs::create_dir_all(&run_dir).unwrap();
    std::fs::create_dir(run_dir.join(".mcmpu-sync.toml")).unwrap();

    let config_error =
        sync_config_for_project_for_gui(config_project.path(), RunSide::Client).unwrap_err();
    assert!(matches!(config_error, RunError::ReadSyncConfig { .. }));

    let defaults_project = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(defaults_project.path().join("overrides")).unwrap();
    std::fs::write(
        defaults_project.path().join("overrides/common"),
        b"not a directory",
    )
    .unwrap();

    let defaults_error =
        sync_config_for_project_for_gui(defaults_project.path(), RunSide::Client).unwrap_err();
    assert!(matches!(defaults_error, RunError::ReadSyncConfig { .. }));
}

#[test]
fn explicit_download_source_orders_primary_first() {
    assert_eq!(
        ordered_download_sources(DownloadSource::Official, false),
        vec![DownloadSource::Official, DownloadSource::Bmclapi]
    );
    assert_eq!(
        ordered_download_sources(DownloadSource::Bmclapi, false),
        vec![DownloadSource::Bmclapi, DownloadSource::Official]
    );
}

#[test]
fn auto_download_source_prefers_bmclapi_for_cn_probe() {
    assert_eq!(
        ordered_download_sources(DownloadSource::Auto, true),
        vec![DownloadSource::Bmclapi, DownloadSource::Official]
    );
    assert_eq!(
        ordered_download_sources(DownloadSource::Auto, false),
        vec![DownloadSource::Official, DownloadSource::Bmclapi]
    );
}

#[test]
fn curseforge_cdn_url_and_file_name_preserve_sort_prefix() {
    let file = file_with_name(6147102, 873867, "!Red-Core-MC-1.8-1.12-0.6.jar");

    assert_eq!(
        curseforge_cdn_artifact_url(&file).unwrap(),
        "https://edge.forgecdn.net/files/6147/102/%21Red-Core-MC-1.8-1.12-0.6.jar"
    );
    assert_eq!(
        run_file_name(&DependencyType::Mod, "red-core", &file),
        "!red-core-873867-6147102.jar"
    );
}

#[test]
fn curseforge_cdn_url_uses_original_name_and_encodes_path_characters() {
    let file = file_with_name(3310627, 482566, "YARCF-0.14(1.12.2).jar");

    assert_eq!(
        curseforge_cdn_artifact_url(&file).unwrap(),
        "https://edge.forgecdn.net/files/3310/627/YARCF-0.14%281.12.2%29.jar"
    );
}

#[test]
fn curseforge_cdn_download_rejects_a_missing_api_key_before_network_io() {
    let downloader = HttpDownloader::new(None).unwrap();
    let error = downloader
        .download_bytes("https://edge.forgecdn.net/files/3310/627/YARCF-0.14%281.12.2%29.jar")
        .unwrap_err();

    assert!(matches!(
        error,
        RunError::MissingCurseForgeDownloadKey { .. }
    ));
}

#[test]
fn curseforge_cdn_batch_rejects_a_missing_api_key_before_network_io() {
    let temp = tempfile::tempdir().unwrap();
    let downloader = HttpDownloader::new(None).unwrap();
    let request = RunDownloadRequest {
        task_id: "curseforge-mod".to_string(),
        url: "https://edge.forgecdn.net/files/3310/627/YARCF.jar".to_string(),
        fallback_urls: vec!["https://mediafilez.forgecdn.net/files/3310/627/YARCF.jar".to_string()],
        target: temp.path().join("YARCF.jar"),
        expected_size: None,
        expected_sha1: None,
        expected_sha256: None,
    };

    let results = downloader.download_many(&[request]);

    assert!(matches!(
        results.as_slice(),
        [Err(RunError::MissingCurseForgeDownloadKey { .. })]
    ));
    assert!(!temp.path().join("YARCF.jar").exists());
}

/// Resource and shader packs keep their uploaded file name.
///
/// The player's game options reference packs by name, and that file ships inside the built
/// archive, so renaming one to `<slug>-<project>-<file>` would leave the option pointing at a
/// file that does not exist.
#[test]
fn run_file_name_preserves_resource_and_shader_pack_names() {
    let pack = file_with_name(8728638, 300317, "Modernity 1.2.zip");
    assert_eq!(
        run_file_name(&DependencyType::ResourcePack, "modernity", &pack),
        "Modernity 1.2.zip"
    );

    let shader = file_with_name(4503123, 367706, "ComplementaryReimagined_r5.3.zip");
    assert_eq!(
        run_file_name(&DependencyType::ShaderPack, "complementary", &shader),
        "ComplementaryReimagined_r5.3.zip"
    );

    // A mod keeps the identity-based name that makes upgrades replace the previous jar.
    let module = file_with_name(873867, 6147102, "Red-Core.jar");
    assert_eq!(
        run_file_name(&DependencyType::Mod, "red-core", &module),
        "red-core-6147102-873867.jar"
    );
}

#[test]
fn run_file_name_keeps_plus_prefix() {
    let file = file_with_name(20, 10, "+++LoadFirst.jar");

    assert_eq!(
        run_file_name(&DependencyType::Mod, "load-first", &file),
        "+++load-first-10-20.jar"
    );
}

#[test]
fn prepare_run_directory_rebuilds_content_dirs_and_preserves_runtime_and_world_data() {
    let temp = tempfile::tempdir().unwrap();
    let project = temp.path();
    support::write_build_project(project);
    std::fs::create_dir_all(project.join("run/client/mods")).unwrap();
    std::fs::create_dir_all(project.join("run/client/natives")).unwrap();
    std::fs::create_dir_all(project.join("run/client/versions/example")).unwrap();
    std::fs::create_dir_all(project.join("run/client/assets/objects/aa")).unwrap();
    std::fs::create_dir_all(project.join("run/client/libraries/example")).unwrap();
    std::fs::create_dir_all(project.join("run/client/saves/world1")).unwrap();
    std::fs::write(project.join("run/client/mods/stale.jar"), "stale").unwrap();
    std::fs::write(project.join("run/client/natives/stale.dll"), "stale").unwrap();
    std::fs::write(
        project.join("run/client/versions/example/example.json"),
        r#"{"id":"example"}"#,
    )
    .unwrap();
    std::fs::write(project.join("run/client/assets/objects/aa/object"), "asset").unwrap();
    std::fs::write(
        project.join("run/client/libraries/example/installed.jar"),
        "installed loader library",
    )
    .unwrap();
    std::fs::write(project.join("run/client/saves/world1/level.dat"), "world").unwrap();
    std::fs::create_dir_all(project.join("debug/common/config")).unwrap();
    std::fs::write(project.join("debug/common/config/debug-only.cfg"), "debug").unwrap();

    let plan =
        prepare_run_directory_with_api(project, &support::FixtureApi, RunSide::Client).unwrap();

    assert!(!project.join("run/client/mods/stale.jar").exists());
    assert!(project.join("run/client/natives/stale.dll").is_file());
    assert!(
        project
            .join("run/client/versions/example/example.json")
            .is_file()
    );
    assert!(
        project
            .join("run/client/assets/objects/aa/object")
            .is_file()
    );
    assert_eq!(
        std::fs::read_to_string(project.join("run/client/libraries/example/installed.jar"))
            .unwrap(),
        "installed loader library"
    );
    assert!(project.join("run/client/saves/world1/level.dat").is_file());
    assert!(project.join("run/client/config/common.json").is_file());
    assert!(project.join("run/client/config/debug-only.cfg").is_file());
    assert!(
        plan.files
            .iter()
            .any(|file| file.path == "mods/common-mod-10-1010.jar")
    );
    assert!(
        plan.files
            .iter()
            .any(|file| file.path == "mods/client-mod-20-1020.jar")
    );
}

#[test]
fn prepare_run_directory_reports_progress() {
    let temp = tempfile::tempdir().unwrap();
    let project = temp.path();
    support::write_build_project(project);
    let mut events = Vec::new();

    prepare_run_directory_with_api_and_progress(
        project,
        &support::FixtureApi,
        RunSide::Client,
        &mut |event| events.push(event.clone()),
    )
    .unwrap();

    assert!(matches!(
        events.first(),
        Some(mcmpu::project::run::RunProgress::ResolvingProject {
            side: RunSide::Client,
            ..
        })
    ));
    assert!(events.iter().any(|event| matches!(
        event,
        mcmpu::project::run::RunProgress::RebuildingRunDirectory {
            side: RunSide::Client,
            ..
        }
    )));
    let overrides_index = events
        .iter()
        .position(|event| {
            matches!(
                event,
                mcmpu::project::run::RunProgress::CopyingOverrides {
                    side: RunSide::Client
                }
            )
        })
        .unwrap();
    let debug_index = events
        .iter()
        .position(|event| {
            matches!(
                event,
                mcmpu::project::run::RunProgress::CopyingDebug {
                    side: RunSide::Client
                }
            )
        })
        .unwrap();
    assert!(overrides_index < debug_index);
    assert!(events.iter().any(|event| matches!(
        event,
        mcmpu::project::run::RunProgress::RunManifestWritten { file_count } if *file_count > 0
    )));
}

#[test]
fn prepare_run_directory_generates_sync_whitelist_and_ignores_debug_source() {
    let temp = tempfile::tempdir().unwrap();
    let project = temp.path();
    support::write_build_project(project);
    std::fs::create_dir_all(project.join("debug/common/scripts")).unwrap();
    std::fs::write(project.join("debug/common/scripts/dev.zs"), "debug").unwrap();

    prepare_run_directory_with_api(project, &support::FixtureApi, RunSide::Client).unwrap();

    let sync = SyncConfig::from_path(project.join("run/client/.mcmpu-sync.toml")).unwrap();

    assert!(sync.include_paths.contains(&"config".to_string()));
    assert!(!sync.include_paths.contains(&"mods".to_string()));
    assert!(!sync.include_paths.contains(&"scripts".to_string()));
}

#[test]
fn prepare_run_directory_records_effective_sources_with_side_overrides_and_debug_blacklist() {
    let temp = tempfile::tempdir().unwrap();
    let project = temp.path();
    support::write_build_project(project);
    std::fs::write(
        project.join("overrides/common/config/layered.cfg"),
        "common",
    )
    .unwrap();
    std::fs::create_dir_all(project.join("overrides/client/config")).unwrap();
    std::fs::write(
        project.join("overrides/client/config/layered.cfg"),
        "client",
    )
    .unwrap();
    std::fs::create_dir_all(project.join("debug/common/config")).unwrap();
    std::fs::write(project.join("debug/common/config/debug.cfg"), "debug").unwrap();
    std::fs::write(
        project.join("debug/common/config/common.json"),
        "debug override",
    )
    .unwrap();

    prepare_run_directory_with_api(project, &support::FixtureApi, RunSide::Client).unwrap();

    let sources =
        RunSourceManifest::from_path(project.join("run/client/.mcmpu-run-sources.json")).unwrap();
    let layered = sources.entry("config/layered.cfg").unwrap();
    assert_eq!(layered.source, RunSourceKind::OverridesClient);
    assert_eq!(layered.target, Some(SyncTarget::Client));
    assert_eq!(
        std::fs::read_to_string(project.join("run/client/config/layered.cfg")).unwrap(),
        "client"
    );

    let debug = sources.entry("config/debug.cfg").unwrap();
    assert_eq!(debug.source, RunSourceKind::DebugCommon);
    assert_eq!(debug.target, None);

    let debug_override = sources.entry("config/common.json").unwrap();
    assert_eq!(debug_override.source, RunSourceKind::DebugCommon);
    assert_eq!(debug_override.target, None);
}

#[test]
fn scan_pending_sync_detects_modified_deleted_and_added_without_auto_writing_overrides() {
    let temp = tempfile::tempdir().unwrap();
    let project = temp.path();
    support::write_build_project(project);
    std::fs::create_dir_all(project.join("overrides/client/config")).unwrap();
    std::fs::write(project.join("overrides/client/config/client.cfg"), "client").unwrap();

    prepare_run_directory_with_api(project, &support::FixtureApi, RunSide::Client).unwrap();
    std::fs::write(
        project.join("run/client/config/common.json"),
        "runtime common",
    )
    .unwrap();
    std::fs::remove_file(project.join("run/client/config/client.cfg")).unwrap();
    std::fs::write(project.join("run/client/config/new.cfg"), "new").unwrap();

    let pending = scan_pending_sync(project, &project.join("run/client"), RunSide::Client).unwrap();

    assert!(project.join(".mcmpu/pending-sync-client.json").is_file());
    assert_eq!(
        std::fs::read_to_string(project.join("overrides/common/config/common.json")).unwrap(),
        "{ \"x\": 1 }"
    );
    assert!(pending.entries.iter().any(|entry| {
        entry.path == "config/common.json"
            && entry.change == PendingSyncChange::Modified
            && entry.target == SyncTarget::Common
    }));
    assert!(pending.entries.iter().any(|entry| {
        entry.path == "config/client.cfg"
            && entry.change == PendingSyncChange::Deleted
            && entry.target == SyncTarget::Client
    }));
    assert!(pending.entries.iter().any(|entry| {
        entry.path == "config/new.cfg"
            && entry.change == PendingSyncChange::Added
            && entry.target == SyncTarget::Common
    }));
}

#[test]
fn scan_pending_sync_ignores_debug_sources_and_generated_dirs() {
    let temp = tempfile::tempdir().unwrap();
    let project = temp.path();
    support::write_build_project(project);
    std::fs::create_dir_all(project.join("debug/common/config")).unwrap();
    std::fs::write(project.join("debug/common/config/debug.cfg"), "debug").unwrap();
    std::fs::create_dir_all(project.join("overrides/common/natives")).unwrap();
    std::fs::write(
        project.join("overrides/common/natives/overridden.dll"),
        "original native",
    )
    .unwrap();

    prepare_run_directory_with_api(project, &support::FixtureApi, RunSide::Client).unwrap();
    std::fs::write(project.join("run/client/config/debug.cfg"), "changed debug").unwrap();
    std::fs::write(project.join("run/client/mods/local.jar"), "generated").unwrap();
    std::fs::write(project.join("run/client/natives/local.dll"), "generated").unwrap();
    std::fs::write(
        project.join("run/client/natives/overridden.dll"),
        "changed native",
    )
    .unwrap();
    std::fs::write(
        project.join("run/client/.mcmpu-sync.toml"),
        "include_paths = [\"config\"]\n",
    )
    .unwrap();

    let pending = scan_pending_sync(project, &project.join("run/client"), RunSide::Client).unwrap();

    assert!(
        pending
            .entries
            .iter()
            .all(|entry| entry.path != "config/debug.cfg")
    );
    assert!(
        pending
            .entries
            .iter()
            .all(|entry| entry.path != "mods/local.jar")
    );
    assert!(
        pending
            .entries
            .iter()
            .all(|entry| entry.path != "natives/local.dll")
    );
    assert!(
        pending
            .entries
            .iter()
            .all(|entry| entry.path != "natives/overridden.dll")
    );
    assert_eq!(pending.ignored_debug, 1);
    assert_eq!(pending.ignored_generated, 1);
}

/// The module download writes into the same directories the overlay layers populate, so the
/// layers must be re-applied last to keep a project override authoritative before launch.
#[test]
fn overlay_layers_applied_last_override_a_downloaded_run_file() {
    struct PayloadDownloader(Vec<u8>);

    impl DownloadBytes for PayloadDownloader {
        fn download_bytes(&self, _url: &str) -> Result<Vec<u8>, RunError> {
            Ok(self.0.clone())
        }
    }

    let temp = tempfile::tempdir().unwrap();
    let project = temp.path();
    support::write_build_project(project);
    let override_bytes = b"project override".to_vec();
    let downloaded_bytes = b"downloaded mod".to_vec();
    std::fs::create_dir_all(project.join("overrides/client/mods")).unwrap();
    std::fs::write(
        project.join("overrides/client/mods/common-mod-10-1010.jar"),
        &override_bytes,
    )
    .unwrap();

    let plan =
        prepare_run_directory_with_api(project, &support::FixtureApi, RunSide::Client).unwrap();
    let target = plan.run_dir.join("mods/common-mod-10-1010.jar");
    assert_eq!(std::fs::read(&target).unwrap(), override_bytes);

    let downloader = PayloadDownloader(downloaded_bytes.clone());
    let files = vec![mcmpu::project::run::RunFile {
        path: "mods/common-mod-10-1010.jar".to_string(),
        url: "https://example.invalid/common-mod.jar".to_string(),
        cache_path: temp.path().join("cache/common-mod.jar"),
        sha1: format!("{:x}", sha1::Sha1::digest(&downloaded_bytes)),
        size: downloaded_bytes.len() as u64,
    }];
    verify_or_download_run_files(&plan.run_dir, &files, &downloader).unwrap();
    assert_eq!(std::fs::read(&target).unwrap(), downloaded_bytes);

    apply_overlay_layers(project, &plan.run_dir, RunSide::Client).unwrap();

    assert_eq!(std::fs::read(&target).unwrap(), override_bytes);
    let sources =
        RunSourceManifest::from_path(plan.run_dir.join(".mcmpu-run-sources.json")).unwrap();
    let entry = sources
        .entries
        .iter()
        .find(|entry| entry.path == "mods/common-mod-10-1010.jar")
        .expect("overlay source entry survives the re-application");
    assert_eq!(
        entry.sha1,
        format!("{:x}", sha1::Sha1::digest(&override_bytes))
    );
}

#[test]
fn apply_pending_sync_writes_updates_additions_and_confirmed_deletions_to_selected_targets() {
    let temp = tempfile::tempdir().unwrap();
    let project = temp.path();
    support::write_build_project(project);
    std::fs::create_dir_all(project.join("overrides/client/config")).unwrap();
    std::fs::write(project.join("overrides/client/config/client.cfg"), "client").unwrap();

    prepare_run_directory_with_api(project, &support::FixtureApi, RunSide::Client).unwrap();
    std::fs::write(
        project.join("run/client/config/common.json"),
        "runtime common",
    )
    .unwrap();
    std::fs::remove_file(project.join("run/client/config/client.cfg")).unwrap();
    std::fs::write(project.join("run/client/config/new.cfg"), "new").unwrap();
    let mut pending = PendingSyncManifest::from_path(
        scan_pending_sync(project, &project.join("run/client"), RunSide::Client)
            .unwrap()
            .path,
    )
    .unwrap();
    for entry in &mut pending.entries {
        entry.apply = true;
        if entry.path == "config/new.cfg" {
            entry.target = SyncTarget::Server;
        }
    }

    let applied = apply_pending_sync(project, &mut pending).unwrap();

    assert!(applied.contains(&"modified config/common.json -> common".to_string()));
    assert!(applied.contains(&"deleted config/client.cfg -> client".to_string()));
    assert!(applied.contains(&"added config/new.cfg -> server".to_string()));
    assert_eq!(
        std::fs::read_to_string(project.join("overrides/common/config/common.json")).unwrap(),
        "runtime common"
    );
    assert!(!project.join("overrides/client/config/client.cfg").exists());
    assert_eq!(
        std::fs::read_to_string(project.join("overrides/server/config/new.cfg")).unwrap(),
        "new"
    );
    assert!(pending.entries.is_empty());
}

#[test]
fn apply_pending_sync_rejects_debug_sources_before_writing_any_override() {
    let temp = tempfile::tempdir().unwrap();
    let project = temp.path();
    support::write_build_project(project);
    std::fs::create_dir_all(project.join("debug/common/config")).unwrap();
    std::fs::write(project.join("debug/common/config/dev.cfg"), "debug").unwrap();

    prepare_run_directory_with_api(project, &support::FixtureApi, RunSide::Client).unwrap();
    std::fs::write(
        project.join("run/client/config/common.json"),
        "changed release file",
    )
    .unwrap();
    let mut pending =
        scan_pending_sync(project, &project.join("run/client"), RunSide::Client).unwrap();
    for entry in &mut pending.entries {
        entry.apply = true;
    }
    pending.entries.push(PendingSyncEntry {
        path: "config/dev.cfg".to_string(),
        change: PendingSyncChange::Modified,
        source: Some(RunSourceKind::DebugCommon),
        target: SyncTarget::Common,
        apply: true,
    });

    let error = apply_pending_sync(project, &mut pending).unwrap_err();

    assert!(matches!(error, RunError::InvalidPendingSyncEntry { .. }));
    assert_eq!(
        std::fs::read_to_string(project.join("overrides/common/config/common.json")).unwrap(),
        "{ \"x\": 1 }"
    );
    assert!(!project.join("overrides/common/config/dev.cfg").exists());
}

#[test]
fn prepare_run_directory_uses_curseforge_cdn_without_curseforge_download_url() {
    let temp = tempfile::tempdir().unwrap();
    let project = temp.path();
    support::write_build_project(project);

    let plan = prepare_run_directory_with_api(project, &NoDownloadUrlApi, RunSide::Client).unwrap();

    assert!(
        plan.files
            .iter()
            .all(|file| file.url.starts_with("https://edge.forgecdn.net/files/"))
    );
}

#[test]
fn cleanroom_run_client_does_not_add_relauncher_variant() {
    let temp = tempfile::tempdir().unwrap();
    let project = temp.path();
    support::write_build_project(project);

    let plan =
        prepare_run_directory_with_api(project, &support::FixtureApi, RunSide::Client).unwrap();

    assert!(
        plan.files
            .iter()
            .all(|file| !file.path.contains("cleanroom-relauncher"))
    );
    assert!(
        plan.files
            .iter()
            .any(|file| file.path == "mods/fugue-1005815-1006815.jar")
    );
}

#[test]
fn verify_or_download_run_files_uses_cache_when_sha1_and_size_match() {
    let temp = tempfile::tempdir().unwrap();
    let project = temp.path();
    support::write_build_project(project);

    let plan =
        prepare_run_directory_with_api(project, &support::FixtureApi, RunSide::Client).unwrap();
    let first = plan
        .files
        .iter()
        .find(|file| file.path == "mods/common-mod-10-1010.jar")
        .unwrap();
    std::fs::create_dir_all(first.cache_path.parent().unwrap()).unwrap();
    std::fs::write(&first.cache_path, b"hello").unwrap();

    let mut files = vec![first.clone()];
    let file = &mut files[0];
    file.size = 5;
    file.sha1 = "aaf4c61ddcc5e8a2dabede0f3b482cd9aea9434d".to_string();

    let downloads = CountingDownloader::new(b"wrong".to_vec());
    verify_or_download_run_files(&plan.run_dir, &files, &downloads).unwrap();

    assert_eq!(downloads.count(), 0);
    assert_eq!(
        std::fs::read(project.join("run/client/mods/common-mod-10-1010.jar")).unwrap(),
        b"hello"
    );
}

#[test]
fn verify_or_download_run_files_preserves_valid_target_mtime_without_cache_io() {
    let temp = tempfile::tempdir().unwrap();
    let plan = launch_plan(temp.path(), RunSide::Client);
    let bytes = b"already installed";
    let target = plan.run_dir.join("mods/example.jar");
    std::fs::create_dir_all(target.parent().unwrap()).unwrap();
    std::fs::write(&target, bytes).unwrap();
    let before = std::fs::metadata(&target).unwrap().modified().unwrap();
    std::thread::sleep(std::time::Duration::from_millis(1100));
    let cache = temp.path().join(".mcmpu/cache/mods/example.jar");
    let files = vec![mcmpu::project::run::RunFile {
        path: "mods/example.jar".to_string(),
        cache_path: cache.clone(),
        url: "https://example.invalid/example.jar".to_string(),
        sha1: format!("{:x}", sha1::Sha1::digest(bytes)),
        size: bytes.len() as u64,
    }];
    let downloads = CountingDownloader::new(b"must not download".to_vec());

    verify_or_download_run_files(&plan.run_dir, &files, &downloads).unwrap();

    assert_eq!(downloads.count(), 0);
    assert!(!cache.exists());
    assert_eq!(
        std::fs::metadata(target).unwrap().modified().unwrap(),
        before
    );
}

#[test]
fn verify_or_download_run_files_replaces_bad_cache() {
    let temp = tempfile::tempdir().unwrap();
    let project = temp.path();
    support::write_build_project(project);

    let plan =
        prepare_run_directory_with_api(project, &support::FixtureApi, RunSide::Client).unwrap();
    let first = plan
        .files
        .iter()
        .find(|file| file.path == "mods/common-mod-10-1010.jar")
        .unwrap();
    std::fs::create_dir_all(first.cache_path.parent().unwrap()).unwrap();
    std::fs::write(&first.cache_path, b"bad").unwrap();

    let mut files = vec![first.clone()];
    files[0].size = 5;
    files[0].sha1 = "aaf4c61ddcc5e8a2dabede0f3b482cd9aea9434d".to_string();

    let downloads = CountingDownloader::new(b"hello".to_vec());
    verify_or_download_run_files(&plan.run_dir, &files, &downloads).unwrap();

    assert_eq!(downloads.count(), 1);
    assert_eq!(std::fs::read(&files[0].cache_path).unwrap(), b"hello");
    assert_eq!(
        std::fs::read(project.join("run/client/mods/common-mod-10-1010.jar")).unwrap(),
        b"hello"
    );
}

#[test]
fn offline_uuid_is_stable_for_same_player_name() {
    assert_eq!(
        offline_uuid_for_player("Player"),
        offline_uuid_for_player("Player")
    );
    assert_eq!(
        offline_uuid_for_player("Player"),
        "a01e3843-e521-3998-958a-f459800e4d11"
    );
}

#[test]
fn verify_or_download_run_files_submits_missing_mods_as_one_batch() {
    struct BatchDownloader {
        payloads: std::collections::BTreeMap<String, Vec<u8>>,
        batch_sizes: std::sync::Mutex<Vec<usize>>,
    }

    impl DownloadBytes for BatchDownloader {
        fn download_bytes(&self, url: &str) -> Result<Vec<u8>, RunError> {
            self.payloads
                .get(url)
                .cloned()
                .ok_or_else(|| RunError::InvalidDownloadBatch {
                    reason: format!("missing fixture for {url}"),
                })
        }

        fn download_many(&self, requests: &[RunDownloadRequest]) -> Vec<Result<(), RunError>> {
            self.batch_sizes.lock().unwrap().push(requests.len());
            let mut results = Vec::with_capacity(requests.len());
            for request in requests {
                let result = match self.download_bytes(&request.url) {
                    Ok(bytes) => {
                        std::fs::create_dir_all(request.target.parent().unwrap()).unwrap();
                        std::fs::write(&request.target, bytes).unwrap();
                        Ok(())
                    }
                    Err(error) => Err(error),
                };
                results.push(result);
            }
            results
        }
    }

    let temp = tempfile::tempdir().unwrap();
    let run_dir = temp.path().join("run/client");
    let first = b"first mod".to_vec();
    let second = b"second mod".to_vec();
    let files = vec![
        mcmpu::project::run::RunFile {
            path: "mods/first.jar".to_string(),
            url: "https://example.invalid/first.jar".to_string(),
            cache_path: temp.path().join("cache/first.jar"),
            sha1: format!("{:x}", sha1::Sha1::digest(&first)),
            size: first.len() as u64,
        },
        mcmpu::project::run::RunFile {
            path: "mods/second.jar".to_string(),
            url: "https://example.invalid/second.jar".to_string(),
            cache_path: temp.path().join("cache/second.jar"),
            sha1: format!("{:x}", sha1::Sha1::digest(&second)),
            size: second.len() as u64,
        },
    ];
    let downloader = BatchDownloader {
        payloads: std::collections::BTreeMap::from([
            (files[0].url.clone(), first),
            (files[1].url.clone(), second),
        ]),
        batch_sizes: std::sync::Mutex::new(Vec::new()),
    };

    verify_or_download_run_files(&run_dir, &files, &downloader).unwrap();

    assert_eq!(*downloader.batch_sizes.lock().unwrap(), vec![2]);
    assert!(run_dir.join("mods/first.jar").is_file());
    assert!(run_dir.join("mods/second.jar").is_file());
}

#[test]
fn run_download_completion_is_reported_before_the_concurrent_batch_returns() {
    struct StreamingDownloader {
        first_report_observed: std::sync::Arc<std::sync::atomic::AtomicBool>,
    }

    impl DownloadBytes for StreamingDownloader {
        fn download_bytes(&self, _url: &str) -> Result<Vec<u8>, RunError> {
            unreachable!("streaming fixture uses the observed batch path")
        }

        fn download_many_observed(
            &self,
            requests: &[RunDownloadRequest],
            observer: std::sync::Arc<dyn TransferObserver>,
        ) -> Vec<Result<(), RunError>> {
            let mut results = Vec::with_capacity(requests.len());
            for (index, request) in requests.iter().enumerate() {
                std::fs::create_dir_all(request.target.parent().unwrap()).unwrap();
                std::fs::write(&request.target, format!("payload-{index}")).unwrap();
                observer.observe(TransferEvent {
                    task_id: request.task_id.clone(),
                    phase: TransferPhase::Single,
                    transferred_bytes: request.expected_size.unwrap(),
                    total_bytes: request.expected_size,
                    active_requests: 2,
                    bytes_per_second: 1024.0,
                });
                observer.observe(TransferEvent {
                    task_id: request.task_id.clone(),
                    phase: TransferPhase::Completed,
                    transferred_bytes: request.expected_size.unwrap(),
                    total_bytes: request.expected_size,
                    active_requests: 1,
                    bytes_per_second: 1024.0,
                });
                if index == 0 {
                    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
                    while !self
                        .first_report_observed
                        .load(std::sync::atomic::Ordering::Acquire)
                    {
                        assert!(
                            std::time::Instant::now() < deadline,
                            "first completion was buffered until the batch returned"
                        );
                        std::thread::sleep(std::time::Duration::from_millis(5));
                    }
                }
                results.push(Ok(()));
            }
            results
        }
    }

    let temp = tempfile::tempdir().unwrap();
    let run_dir = temp.path().join("run/client");
    let files = (0..2)
        .map(|index| {
            let bytes = format!("payload-{index}").into_bytes();
            mcmpu::project::run::RunFile {
                path: format!("mods/file-{index}.jar"),
                url: format!("https://example.invalid/file-{index}.jar"),
                cache_path: temp.path().join(format!("cache/file-{index}.jar")),
                sha1: format!("{:x}", sha1::Sha1::digest(&bytes)),
                size: bytes.len() as u64,
            }
        })
        .collect::<Vec<_>>();
    let first_report_observed = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let downloader = StreamingDownloader {
        first_report_observed: std::sync::Arc::clone(&first_report_observed),
    };
    let mut events = Vec::new();

    verify_or_download_run_files_with_progress(&run_dir, &files, &downloader, &mut |event| {
        if matches!(
            event,
            RunProgress::FileReady {
                completed: 1,
                source: RunFileReadySource::Downloaded,
                ..
            }
        ) {
            first_report_observed.store(true, std::sync::atomic::Ordering::Release);
        }
        events.push(event.clone());
    })
    .unwrap();

    assert!(first_report_observed.load(std::sync::atomic::Ordering::Acquire));
    assert!(
        events
            .iter()
            .any(|event| matches!(event, RunProgress::FileDownloadStarted { .. }))
    );
    assert!(
        events
            .iter()
            .any(|event| matches!(event, RunProgress::DownloadProgress { completed: 2, .. }))
    );
}

#[test]
fn resolve_java_executable_prefers_explicit_config_path() {
    let temp = tempfile::tempdir().unwrap();
    let java = temp.path().join("bin/java.exe");
    std::fs::create_dir_all(java.parent().unwrap()).unwrap();
    std::fs::write(&java, "").unwrap();
    let config = TestRunConfig {
        java_path: java.display().to_string(),
        ..TestRunConfig::default()
    };

    let resolved = resolve_java_executable(temp.path(), &config, 8, &EmptyJavaLocator).unwrap();

    assert_eq!(resolved, java);
}

#[test]
fn resolve_java_executable_uses_locator_before_project_java_dir() {
    let temp = tempfile::tempdir().unwrap();
    let path_java = temp.path().join("path/java");
    std::fs::create_dir_all(path_java.parent().unwrap()).unwrap();
    std::fs::write(&path_java, "").unwrap();
    let project_java = temp
        .path()
        .join(format!(".mcmpu/java/8/bin/{}", test_java_executable_name()));
    std::fs::create_dir_all(project_java.parent().unwrap()).unwrap();
    std::fs::write(&project_java, "").unwrap();

    let resolved = resolve_java_executable(
        temp.path(),
        &TestRunConfig::default(),
        8,
        &FixedJavaLocator {
            path: Some(path_java.clone()),
            common: None,
        },
    )
    .unwrap();

    assert_eq!(resolved, path_java);
}

#[test]
fn resolve_java_executable_uses_common_path_before_project_java_dir() {
    let temp = tempfile::tempdir().unwrap();
    let common_java = temp.path().join("common/java");
    std::fs::create_dir_all(common_java.parent().unwrap()).unwrap();
    std::fs::write(&common_java, "").unwrap();
    let project_java = temp
        .path()
        .join(format!(".mcmpu/java/8/bin/{}", test_java_executable_name()));
    std::fs::create_dir_all(project_java.parent().unwrap()).unwrap();
    std::fs::write(&project_java, "").unwrap();

    let resolved = resolve_java_executable(
        temp.path(),
        &TestRunConfig::default(),
        8,
        &FixedJavaLocator {
            path: None,
            common: Some(common_java.clone()),
        },
    )
    .unwrap();

    assert_eq!(resolved, common_java);
}

#[test]
fn resolve_java_executable_uses_project_java_dir_after_locator() {
    let temp = tempfile::tempdir().unwrap();
    let project_java = temp
        .path()
        .join(format!(".mcmpu/java/8/bin/{}", test_java_executable_name()));
    std::fs::create_dir_all(project_java.parent().unwrap()).unwrap();
    std::fs::write(&project_java, "").unwrap();

    let resolved = resolve_java_executable(
        temp.path(),
        &TestRunConfig::default(),
        8,
        &FixedJavaLocator {
            path: None,
            common: None,
        },
    )
    .unwrap();

    assert_eq!(resolved, project_java);
}

#[test]
fn resolve_java_executable_reports_major_version_when_missing() {
    let temp = tempfile::tempdir().unwrap();
    let config = TestRunConfig::default();

    let err = resolve_java_executable(temp.path(), &config, 21, &EmptyJavaLocator).unwrap_err();

    let message = err.to_string();
    assert!(message.contains("major 21"));
    assert!(message.contains("version 21"));
}

#[test]
fn resolve_or_download_java_installs_verified_zulu_archive() {
    let temp = tempfile::tempdir().unwrap();
    let executable = test_java_executable_name();
    let mut archive = std::io::Cursor::new(Vec::new());
    {
        let mut zip = zip::ZipWriter::new(&mut archive);
        zip.start_file(
            format!("zulu-test/bin/{executable}"),
            zip::write::SimpleFileOptions::default(),
        )
        .unwrap();
        std::io::Write::write_all(&mut zip, b"fixture java").unwrap();
        zip.finish().unwrap();
    }
    let archive = archive.into_inner();
    let archive_hash = format!("{:x}", Sha256::digest(&archive));
    let downloader = ZuluDownloader {
        metadata: format!(
            r#"[{{"download_url":"https://example.invalid/zulu.zip","sha256_hash":"{archive_hash}","name":"zulu-test.zip"}}]"#
        )
        .into_bytes(),
        archive,
        count: std::sync::atomic::AtomicUsize::new(0),
    };
    let config = TestRunConfig::default();

    let java = resolve_or_download_java_executable(
        temp.path(),
        &config,
        17,
        &EmptyJavaLocator,
        &downloader,
    )
    .unwrap();

    assert_eq!(downloader.count(), 2);
    assert_eq!(
        java,
        temp.path()
            .join(format!(".mcmpu/java/17/runtime/zulu-test/bin/{executable}"))
    );
    assert_eq!(std::fs::read(&java).unwrap(), b"fixture java");

    let reused = resolve_or_download_java_executable(
        temp.path(),
        &config,
        17,
        &EmptyJavaLocator,
        &downloader,
    )
    .unwrap();
    assert_eq!(reused, java);
    assert_eq!(downloader.count(), 2);

    std::fs::remove_dir_all(temp.path().join(".mcmpu/java/17/runtime")).unwrap();
    let restored = resolve_or_download_java_executable(
        temp.path(),
        &config,
        17,
        &EmptyJavaLocator,
        &downloader,
    )
    .unwrap();
    assert_eq!(std::fs::read(restored).unwrap(), b"fixture java");
    assert_eq!(downloader.count(), 2);
}

#[test]
fn bad_zulu_checksum_does_not_publish_a_runtime() {
    let temp = tempfile::tempdir().unwrap();
    let archive = zulu_fixture_archive(b"fixture java");
    let downloader = ZuluDownloader {
        metadata: br#"[{"download_url":"https://example.invalid/zulu.zip","sha256_hash":"0000000000000000000000000000000000000000000000000000000000000000","name":"zulu-test.zip"}]"#.to_vec(),
        archive,
        count: std::sync::atomic::AtomicUsize::new(0),
    };
    let config = TestRunConfig::default();

    let error = resolve_or_download_java_executable(
        temp.path(),
        &config,
        17,
        &EmptyJavaLocator,
        &downloader,
    )
    .unwrap_err();

    assert!(matches!(error, RunError::ZuluChecksum { .. }));
    assert!(!temp.path().join(".mcmpu/java/17/runtime").exists());
    assert!(
        !walkdir::WalkDir::new(temp.path().join(".mcmpu/cache/java/17"))
            .into_iter()
            .filter_map(Result::ok)
            .any(|entry| entry.path().extension().is_some_and(|value| value == "zip"))
    );
}

#[test]
fn interrupted_zulu_download_does_not_publish_or_damage_runtime() {
    let temp = tempfile::tempdir().unwrap();
    let hash = "1".repeat(64);
    let downloader = InterruptedZuluDownloader {
        metadata: format!(
            r#"[{{"download_url":"https://example.invalid/zulu.zip","sha256_hash":"{hash}","name":"zulu-test.zip"}}]"#
        )
        .into_bytes(),
    };
    let config = TestRunConfig::default();

    let error = resolve_or_download_java_executable(
        temp.path(),
        &config,
        17,
        &EmptyJavaLocator,
        &downloader,
    )
    .unwrap_err();

    assert!(error.to_string().contains("interrupted fixture download"));
    assert!(!temp.path().join(".mcmpu/java/17/runtime").exists());
    assert!(
        walkdir::WalkDir::new(temp.path().join(".mcmpu/cache/java/17"))
            .into_iter()
            .filter_map(Result::ok)
            .all(|entry| !entry.file_name().to_string_lossy().contains("part"))
    );
}

#[test]
fn zulu_recovery_reuses_a_crash_left_runtime_backup_and_unlocked_lock_file() {
    let temp = tempfile::tempdir().unwrap();
    let executable = test_java_executable_name();
    let cache_root = temp
        .path()
        .join(".mcmpu/cache/java/17")
        .join(test_zulu_platform_cache_key());
    std::fs::create_dir_all(&cache_root).unwrap();
    std::fs::write(cache_root.join("prepare.lock"), b"pid=stale-process\n").unwrap();

    let runtime_parent = temp.path().join(".mcmpu/java/17");
    let backup = runtime_parent.join(".runtime.backup-crashed-install");
    let recovered_java = backup.join("zulu/bin").join(executable);
    std::fs::create_dir_all(recovered_java.parent().unwrap()).unwrap();
    std::fs::write(&recovered_java, b"previous verified runtime").unwrap();

    let stale_part = runtime_parent.join(".runtime.part-crashed-install");
    std::fs::create_dir_all(&stale_part).unwrap();
    std::fs::write(stale_part.join("partial"), b"partial runtime").unwrap();

    let downloads = CountingDownloader::new(Vec::new());
    let config = TestRunConfig::default();
    let java = resolve_or_download_java_executable(
        temp.path(),
        &config,
        17,
        &EmptyJavaLocator,
        &downloads,
    )
    .unwrap();

    assert_eq!(
        java,
        runtime_parent.join("runtime/zulu/bin").join(executable)
    );
    assert_eq!(std::fs::read(&java).unwrap(), b"previous verified runtime");
    assert_eq!(downloads.count(), 0);
    assert!(!backup.exists());
    assert!(!stale_part.exists());
}

#[test]
fn concurrent_zulu_preparers_share_one_metadata_and_archive_download() {
    let temp = tempfile::tempdir().unwrap();
    let archive = zulu_fixture_archive(b"fixture java");
    let hash = format!("{:x}", Sha256::digest(&archive));
    let downloader = std::sync::Arc::new(SlowZuluDownloader {
        metadata: format!(
            r#"[{{"download_url":"https://example.invalid/zulu.zip","sha256_hash":"{hash}","name":"zulu-test.zip"}}]"#
        )
        .into_bytes(),
        archive,
        count: std::sync::atomic::AtomicUsize::new(0),
    });
    let project = std::sync::Arc::new(temp.path().to_path_buf());
    let mut handles = Vec::new();
    for _ in 0..2 {
        let downloader = std::sync::Arc::clone(&downloader);
        let project = std::sync::Arc::clone(&project);
        handles.push(std::thread::spawn(move || {
            let config = TestRunConfig::default();
            resolve_or_download_java_executable(
                project.as_path(),
                &config,
                17,
                &EmptyJavaLocator,
                downloader.as_ref(),
            )
            .map_err(|error| error.to_string())
        }));
    }
    let first = handles.remove(0).join().unwrap().unwrap();
    let second = handles.remove(0).join().unwrap().unwrap();

    assert_eq!(first, second);
    assert_eq!(downloader.count(), 2);
}

#[test]
fn zulu_prepare_lock_coordinates_with_another_process() {
    let temp = tempfile::tempdir().unwrap();
    let ready = temp.path().join("lock-ready");
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "zulu_prepare_lock_child_fixture", "--nocapture"])
        .env("MCMPU_TEST_ZULU_LOCK_ROOT", temp.path())
        .env("MCMPU_TEST_ZULU_LOCK_READY", &ready)
        .spawn()
        .unwrap();
    let wait_started = std::time::Instant::now();
    while !ready.is_file() && wait_started.elapsed() < std::time::Duration::from_secs(5) {
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    assert!(ready.is_file());

    let archive = zulu_fixture_archive(b"fixture java");
    let hash = format!("{:x}", Sha256::digest(&archive));
    let downloader = ZuluDownloader {
        metadata: format!(
            r#"[{{"download_url":"https://example.invalid/zulu.zip","sha256_hash":"{hash}","name":"zulu-test.zip"}}]"#
        )
        .into_bytes(),
        archive,
        count: std::sync::atomic::AtomicUsize::new(0),
    };
    let config = TestRunConfig::default();
    let started = std::time::Instant::now();
    let result = resolve_or_download_java_executable(
        temp.path(),
        &config,
        17,
        &EmptyJavaLocator,
        &downloader,
    )
    .unwrap();

    assert!(started.elapsed() >= std::time::Duration::from_millis(300));
    assert!(result.is_file());
    assert!(child.wait().unwrap().success());
}

#[test]
fn zulu_prepare_lock_child_fixture() {
    let Some(root) = std::env::var_os("MCMPU_TEST_ZULU_LOCK_ROOT") else {
        return;
    };
    let ready = std::path::PathBuf::from(
        std::env::var_os("MCMPU_TEST_ZULU_LOCK_READY").expect("child ready path must be set"),
    );
    let lock = std::path::PathBuf::from(root)
        .join(".mcmpu/cache/java/17")
        .join(test_zulu_platform_cache_key())
        .join("prepare.lock");
    std::fs::create_dir_all(lock.parent().unwrap()).unwrap();
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&lock)
        .unwrap();
    file.lock().unwrap();
    std::io::Write::write_all(&mut file, b"external process lock").unwrap();
    file.sync_all().unwrap();
    std::fs::write(ready, b"ready").unwrap();
    std::thread::sleep(std::time::Duration::from_millis(500));
    drop(file);
    std::fs::remove_file(lock).unwrap();
}

#[test]
fn legacy_plan_launch_command_requires_a_descriptor() {
    let temp = tempfile::tempdir().unwrap();
    let plan = launch_plan(temp.path(), RunSide::Client);

    let error = plan_launch_command(&plan, std::path::Path::new("java")).unwrap_err();

    assert!(matches!(
        error,
        mcmpu::project::run::RunError::LaunchDescriptorRequired
    ));
}

#[test]
fn descriptor_launch_command_builds_real_client_arguments_with_offline_identity() {
    let temp = tempfile::tempdir().unwrap();
    let plan = launch_plan(temp.path(), RunSide::Client);
    let mut descriptor = launch_descriptor(LaunchSide::Client);
    descriptor.jvm_arguments = vec![
        "-Djava.library.path=${natives_directory}".to_string(),
        "-cp".to_string(),
        "${classpath}".to_string(),
    ];
    descriptor.game_arguments = vec![
        "--username".to_string(),
        "${auth_player_name}".to_string(),
        "--uuid".to_string(),
        "${auth_uuid}".to_string(),
        "--accessToken".to_string(),
        "${auth_access_token}".to_string(),
    ];

    let command =
        plan_launch_command_from_descriptor(&plan, &descriptor, std::path::Path::new("java"))
            .unwrap();

    assert_eq!(command.side, RunSide::Client);
    assert_eq!(command.args[0], "-Xmx4G");
    assert_eq!(
        command
            .args
            .iter()
            .filter(|argument| argument.as_str() == "-cp")
            .count(),
        1
    );
    assert!(command.args.contains(&"example.Main".to_string()));
    assert!(command.args.contains(&"Player".to_string()));
    assert!(command.args.contains(&offline_uuid_for_player("Player")));
    assert!(command.args.contains(&"0".to_string()));
    assert!(command.args.iter().any(|argument| {
        argument.contains("libraries/example.jar") || argument.contains("libraries\\example.jar")
    }));
}

#[test]
fn descriptor_launch_command_uses_minecraft_version_for_loader_profile_variables() {
    let temp = tempfile::tempdir().unwrap();
    let plan = launch_plan(temp.path(), RunSide::Client);
    let mut descriptor = launch_descriptor(LaunchSide::Client);
    descriptor.version_id = "neoforge-21.1.248".to_string();
    descriptor.inheritance_chain = vec!["1.21.1".to_string(), "neoforge-21.1.248".to_string()];
    descriptor.jvm_arguments = vec!["-DignoreList=client-extra,${version_name}.jar".to_string()];

    let command =
        plan_launch_command_from_descriptor(&plan, &descriptor, std::path::Path::new("java"))
            .unwrap();

    assert!(
        command
            .args
            .contains(&"-DignoreList=client-extra,1.21.1.jar".to_string())
    );
    assert!(
        !command
            .args
            .contains(&"-DignoreList=client-extra,neoforge-21.1.248.jar".to_string())
    );
}

#[test]
fn descriptor_launch_command_adds_missing_client_native_library_path() {
    let temp = tempfile::tempdir().unwrap();
    let plan = launch_plan(temp.path(), RunSide::Client);
    let descriptor = launch_descriptor(LaunchSide::Client);

    let command =
        plan_launch_command_from_descriptor(&plan, &descriptor, std::path::Path::new("java"))
            .unwrap();

    let native_arguments = command
        .args
        .iter()
        .filter(|argument| argument.starts_with("-Djava.library.path="))
        .collect::<Vec<_>>();
    assert_eq!(native_arguments.len(), 1);
    assert!(native_arguments[0].ends_with("natives"));
}

#[test]
fn descriptor_launch_command_adds_server_classpath_main_class_and_missing_nogui() {
    let temp = tempfile::tempdir().unwrap();
    let plan = launch_plan(temp.path(), RunSide::Server);
    let descriptor = launch_descriptor(LaunchSide::Server);

    let command =
        plan_launch_command_from_descriptor(&plan, &descriptor, std::path::Path::new("java"))
            .unwrap();

    assert_eq!(command.args[0], "-Xmx4G");
    assert_eq!(command.args[1], "-cp");
    assert_eq!(command.args[3], "example.Main");
    assert_eq!(command.args.last().unwrap(), "nogui");
}

#[test]
fn descriptor_launch_command_does_not_duplicate_server_nogui() {
    let temp = tempfile::tempdir().unwrap();
    let plan = launch_plan(temp.path(), RunSide::Server);
    let mut descriptor = launch_descriptor(LaunchSide::Server);
    descriptor.game_arguments = vec!["--nogui".to_string()];

    let command =
        plan_launch_command_from_descriptor(&plan, &descriptor, std::path::Path::new("java"))
            .unwrap();

    assert_eq!(
        command
            .args
            .iter()
            .filter(|argument| matches!(argument.as_str(), "nogui" | "--nogui"))
            .count(),
        1
    );
}

#[test]
fn descriptor_launch_command_rejects_a_missing_run_directory() {
    let temp = tempfile::tempdir().unwrap();
    let plan = mcmpu::project::run::RunLaunchPlan {
        side: RunSide::Client,
        run_dir: temp.path().join("missing/run/client"),
        java_args: Vec::new(),
        player_name: "Player".to_string(),
        offline_uuid: offline_uuid_for_player("Player"),
        files: Vec::new(),
    };

    let error = plan_launch_command_from_descriptor(
        &plan,
        &launch_descriptor(LaunchSide::Client),
        std::path::Path::new("java"),
    )
    .unwrap_err();

    assert!(matches!(
        error,
        RunError::LaunchRunDirectory { path, .. } if path == plan.run_dir
    ));
}

#[test]
fn descriptor_launch_command_uses_one_absolute_root_and_loads_main_from_libraries() {
    let workspace = std::env::current_dir().unwrap();
    let temp = tempfile::tempdir_in(&workspace).unwrap();
    let project = temp.path().join("project with spaces");
    let run_dir = project.join("run/client");
    let libraries = run_dir.join("libraries");
    let source_dir = temp.path().join("java source/fixture");
    std::fs::create_dir_all(&libraries).unwrap();
    std::fs::create_dir_all(&source_dir).unwrap();
    std::fs::create_dir_all(run_dir.join("assets")).unwrap();
    std::fs::create_dir_all(run_dir.join("natives")).unwrap();
    let source = source_dir.join("Main.java");
    std::fs::write(
        &source,
        r#"package fixture;

import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.nio.file.Path;
import java.nio.file.Paths;

public final class Main {
    public static void main(String[] args) throws Exception {
        Path game = Paths.get(args[0]).toAbsolutePath().normalize();
        Path current = Paths.get("").toAbsolutePath().normalize();
        require(game.equals(current), "working directory does not match game directory");
        require(Paths.get(args[1]).normalize().equals(game.resolve("assets")), "assets root mismatch");
        require(Paths.get(args[2]).normalize().equals(game.resolve("natives")), "natives directory mismatch");
        require(Paths.get(args[3]).normalize().equals(game.resolve("libraries")), "library directory mismatch");
        Files.write(Paths.get(args[4]), "loaded".getBytes(StandardCharsets.UTF_8));
    }

    private static void require(boolean condition, String message) {
        if (!condition) {
            throw new IllegalStateException(message);
        }
    }
}
"#,
    )
    .unwrap();
    let compilation = std::process::Command::new("javac")
        .args(["-source", "8", "-target", "8", "-d"])
        .arg(&libraries)
        .arg(&source)
        .output()
        .unwrap();
    assert!(
        compilation.status.success(),
        "fixture compilation failed: {}",
        String::from_utf8_lossy(&compilation.stderr)
    );

    let relative_project = project.strip_prefix(&workspace).unwrap();
    let plan = launch_plan(relative_project, RunSide::Client);
    let mut descriptor = launch_descriptor(LaunchSide::Client);
    descriptor.main_class = "fixture.Main".to_string();
    descriptor.classpath = vec!["libraries".to_string()];
    descriptor.jvm_arguments = vec!["-cp".to_string(), "${classpath}".to_string()];
    descriptor.game_arguments = vec![
        "${game_directory}".to_string(),
        "${assets_root}".to_string(),
        "${natives_directory}".to_string(),
        "${library_directory}".to_string(),
        "${game_directory}/main-loaded.txt".to_string(),
    ];

    let command =
        plan_launch_command_from_descriptor(&plan, &descriptor, std::path::Path::new("java"))
            .unwrap();
    assert!(command.working_dir.is_absolute());
    assert_eq!(
        std::fs::canonicalize(&command.working_dir).unwrap(),
        std::fs::canonicalize(&run_dir).unwrap()
    );
    let absolute_run_dir = command.working_dir.clone();
    assert!(command.executable.is_absolute());
    let classpath_index = command
        .args
        .iter()
        .position(|argument| argument == "-cp")
        .unwrap();
    assert_eq!(
        std::path::PathBuf::from(&command.args[classpath_index + 1]),
        absolute_run_dir.join("libraries")
    );
    let main_index = command
        .args
        .iter()
        .position(|argument| argument == "fixture.Main")
        .unwrap();
    for argument in &command.args[main_index + 1..] {
        assert!(std::path::Path::new(argument).is_absolute());
    }

    let output = std::process::Command::new(&command.executable)
        .args(&command.args)
        .current_dir(&command.working_dir)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "fixture launch failed:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        std::fs::read_to_string(absolute_run_dir.join("main-loaded.txt")).unwrap(),
        "loaded"
    );
}

#[test]
fn supervise_run_polls_until_exit_and_writes_pending_sync_for_nonzero_status() {
    let temp = tempfile::tempdir().unwrap();
    let project = temp.path();
    support::write_build_project(project);
    prepare_run_directory_with_api(project, &support::FixtureApi, RunSide::Client).unwrap();
    let run_dir = project.join("run/client");
    std::fs::write(run_dir.join("config/runtime.cfg"), "runtime").unwrap();
    let command = LaunchCommand {
        side: RunSide::Client,
        working_dir: run_dir.clone(),
        executable: "java".into(),
        args: vec!["--username".to_string(), "Player".to_string()],
    };
    let runner = FakeRunner::new(vec![None, Some(exit_status(7))], exit_status(7));
    let mut events = Vec::new();

    let outcome = supervise_run(
        project,
        &command,
        &runner,
        &NeverStop,
        &NeverConsole,
        std::time::Duration::from_secs(1),
        &mut |event| events.push(event.clone()),
    )
    .unwrap();

    assert_eq!(outcome.pid, 4242);
    assert_eq!(outcome.status.code(), Some(7));
    assert_eq!(outcome.termination, RunTermination::Exited);
    assert_eq!(outcome.pending_sync.entries.len(), 1);
    assert!(outcome.pending_sync.path.is_file());
    assert!(!project.join("overrides/common/config/runtime.cfg").exists());
    assert!(matches!(events.first(), Some(RunEvent::Spawning { .. })));
    assert!(events.iter().any(|event| matches!(
        event,
        RunEvent::Exited {
            pid: 4242,
            success: false
        }
    )));
    assert!(matches!(
        events.last(),
        Some(RunEvent::PendingSyncWritten { .. })
    ));
}

#[test]
fn supervise_run_sends_stop_to_server_and_uses_client_termination() {
    let temp = tempfile::tempdir().unwrap();
    let project = temp.path();
    support::write_build_project(project);
    prepare_run_directory_with_api(project, &support::FixtureApi, RunSide::Server).unwrap();
    let server_command = LaunchCommand {
        side: RunSide::Server,
        working_dir: project.join("run/server"),
        executable: "java".into(),
        args: Vec::new(),
    };
    let server_runner = FakeRunner::new(vec![None, Some(exit_status(0))], exit_status(0));

    let server_outcome = supervise_run(
        project,
        &server_command,
        &server_runner,
        &AlwaysStop,
        &NeverConsole,
        std::time::Duration::from_secs(1),
        &mut |_| {},
    )
    .unwrap();

    assert_eq!(server_outcome.termination, RunTermination::StopRequested);
    assert_eq!(server_runner.stdin_bytes(), b"stop\n");
    assert_eq!(server_runner.terminate_count(), 0);

    prepare_run_directory_with_api(project, &support::FixtureApi, RunSide::Client).unwrap();
    let client_command = LaunchCommand {
        side: RunSide::Client,
        working_dir: project.join("run/client"),
        executable: "java".into(),
        args: Vec::new(),
    };
    let client_runner = FakeRunner::new(vec![None, Some(exit_status(0))], exit_status(0));

    let client_outcome = supervise_run(
        project,
        &client_command,
        &client_runner,
        &AlwaysStop,
        &NeverConsole,
        std::time::Duration::from_secs(1),
        &mut |_| {},
    )
    .unwrap();

    assert_eq!(client_outcome.termination, RunTermination::StopRequested);
    assert!(client_runner.stdin_bytes().is_empty());
    assert_eq!(client_runner.terminate_count(), 1);
}

#[test]
fn supervise_run_force_terminates_after_server_stop_timeout_and_still_scans_sync() {
    let temp = tempfile::tempdir().unwrap();
    let project = temp.path();
    support::write_build_project(project);
    prepare_run_directory_with_api(project, &support::FixtureApi, RunSide::Server).unwrap();
    let run_dir = project.join("run/server");
    std::fs::write(run_dir.join("config/runtime.cfg"), "runtime").unwrap();
    let command = LaunchCommand {
        side: RunSide::Server,
        working_dir: run_dir,
        executable: "java".into(),
        args: Vec::new(),
    };
    let runner = FakeRunner::new(vec![None], exit_status(9));
    let mut events = Vec::new();

    let outcome = supervise_run(
        project,
        &command,
        &runner,
        &AlwaysStop,
        &NeverConsole,
        std::time::Duration::ZERO,
        &mut |event| events.push(event.clone()),
    )
    .unwrap();

    assert_eq!(outcome.termination, RunTermination::Forced);
    assert_eq!(outcome.status.code(), Some(9));
    assert_eq!(runner.stdin_bytes(), b"stop\n");
    assert_eq!(runner.terminate_count(), 1);
    assert!(outcome.pending_sync.path.is_file());
    assert!(
        events
            .iter()
            .any(|event| matches!(event, RunEvent::ForcedTermination { pid: 4242 }))
    );
}

#[test]
fn run_launch_cancellation_still_writes_pending_sync() {
    let temp = tempfile::tempdir().unwrap();
    let project = temp.path();
    support::write_build_project(project);
    prepare_run_directory_with_api(project, &support::FixtureApi, RunSide::Client).unwrap();
    let run_dir = project.join("run/client");
    std::fs::write(run_dir.join("config/runtime.cfg"), "runtime").unwrap();
    let command = LaunchCommand {
        side: RunSide::Client,
        working_dir: run_dir,
        executable: "java".into(),
        args: Vec::new(),
    };
    let runner = FakeRunner::new(vec![None, Some(exit_status(0))], exit_status(0));

    let result = run_launch_command_with_stop_signal(project, &command, &runner, &AlwaysStop);

    assert!(matches!(
        result,
        Err(mcmpu::project::run::RunError::Cancelled)
    ));
    let pending =
        PendingSyncManifest::from_path(project.join(".mcmpu/pending-sync-client.json")).unwrap();
    assert_eq!(pending.entries.len(), 1);
    assert_eq!(runner.terminate_count(), 1);
}

#[test]
fn pending_sync_lifecycle_prelaunch_failure_after_source_snapshot_still_writes_manifest() {
    let temp = tempfile::tempdir().unwrap();
    let project = temp.path();
    support::write_build_project(project);
    prepare_run_directory_with_api(project, &support::FixtureApi, RunSide::Client).unwrap();
    let run_dir = project.join("run/client");
    std::fs::write(
        run_dir.join("config/prelaunch.cfg"),
        "changed before Java spawn",
    )
    .unwrap();

    let error = finalize_pending_sync_after_run::<()>(
        project,
        &run_dir,
        RunSide::Client,
        Err(RunError::InvalidLaunchDescriptor {
            reason: "prelaunch fixture failure".to_string(),
        }),
    )
    .unwrap_err();

    assert!(matches!(error, RunError::InvalidLaunchDescriptor { .. }));
    let pending =
        PendingSyncManifest::from_path(project.join(".mcmpu/pending-sync-client.json")).unwrap();
    assert!(pending.entries.iter().any(|entry| {
        entry.path == "config/prelaunch.cfg" && entry.change == PendingSyncChange::Added
    }));
}

#[test]
fn pending_sync_lifecycle_process_spawn_failure_still_writes_manifest() {
    let temp = tempfile::tempdir().unwrap();
    let project = temp.path();
    support::write_build_project(project);
    prepare_run_directory_with_api(project, &support::FixtureApi, RunSide::Client).unwrap();
    let run_dir = project.join("run/client");
    std::fs::write(run_dir.join("config/spawn-failed.cfg"), "runtime").unwrap();
    let command = LaunchCommand {
        side: RunSide::Client,
        working_dir: run_dir,
        executable: "missing-java".into(),
        args: Vec::new(),
    };
    let mut events = Vec::new();

    let error = supervise_run(
        project,
        &command,
        &FailingRunner,
        &NeverStop,
        &NeverConsole,
        std::time::Duration::from_secs(1),
        &mut |event| events.push(event.clone()),
    )
    .unwrap_err();

    assert!(matches!(
        error,
        RunError::Process(ProcessError::Start { .. })
    ));
    let pending =
        PendingSyncManifest::from_path(project.join(".mcmpu/pending-sync-client.json")).unwrap();
    assert_eq!(pending.entries.len(), 1);
    assert!(matches!(
        events.last(),
        Some(RunEvent::PendingSyncWritten { entries: 1, .. })
    ));
}

#[test]
fn pending_sync_lifecycle_primary_error_and_previous_manifest_survive_scan_failure() {
    let temp = tempfile::tempdir().unwrap();
    let project = temp.path();
    support::write_build_project(project);
    prepare_run_directory_with_api(project, &support::FixtureApi, RunSide::Client).unwrap();
    let run_dir = project.join("run/client");
    std::fs::write(run_dir.join("config/retained.cfg"), "runtime").unwrap();
    let original = scan_pending_sync(project, &run_dir, RunSide::Client).unwrap();
    assert_eq!(original.entries.len(), 1);
    std::fs::write(run_dir.join(".mcmpu-sync.toml"), "include_paths = [").unwrap();

    let error = finalize_pending_sync_after_run::<()>(
        project,
        &run_dir,
        RunSide::Client,
        Err(RunError::InvalidLaunchDescriptor {
            reason: "primary lifecycle failure".to_string(),
        }),
    )
    .unwrap_err();

    let RunError::LifecycleAndPendingSync {
        lifecycle,
        pending_sync,
    } = error
    else {
        panic!("expected combined lifecycle and pending-sync error");
    };
    assert!(matches!(
        *lifecycle,
        RunError::InvalidLaunchDescriptor { .. }
    ));
    assert!(matches!(*pending_sync, RunError::ParseSyncConfig { .. }));
    let retained =
        PendingSyncManifest::from_path(project.join(".mcmpu/pending-sync-client.json")).unwrap();
    assert_eq!(retained.entries, original.entries);
}

#[test]
fn console_lines_typed_while_running_reach_the_process_stdin() {
    let temp = tempfile::tempdir().unwrap();
    let project = temp.path();
    support::write_build_project(project);
    prepare_run_directory_with_api(project, &support::FixtureApi, RunSide::Server).unwrap();
    let command = LaunchCommand {
        side: RunSide::Server,
        working_dir: project.join("run/server"),
        executable: "java".into(),
        args: Vec::new(),
    };
    let runner = FakeRunner::new(vec![Some(exit_status(0))], exit_status(0));
    let console = SharedConsoleInput::default();
    console.submit("say hello");
    console.submit("   ");
    console.submit(
        "stop
",
    );

    supervise_run(
        project,
        &command,
        &runner,
        &NeverStop,
        &console,
        std::time::Duration::from_secs(1),
        &mut |_| {},
    )
    .unwrap();

    assert_eq!(
        String::from_utf8(runner.stdin_bytes()).unwrap(),
        "say hello\nstop\n"
    );
}

/// A run must end even when the process it started leaves a child holding the output pipes.
/// Waiting for those pipes to close is what left the console reported as running forever.
#[test]
fn supervised_run_ends_even_when_a_child_process_keeps_the_output_pipes_open() {
    let temp = tempfile::tempdir().unwrap();
    let project = temp.path();
    support::write_build_project(project);
    prepare_run_directory_with_api(project, &support::FixtureApi, RunSide::Server).unwrap();
    // Both commands start a background process that inherits stdout/stderr and then exit, so the
    // pipes stay open after the supervised process is gone.
    let (executable, args): (&str, Vec<&str>) = if cfg!(windows) {
        (
            "cmd",
            vec![
                "/C",
                "start",
                "/B",
                "ping",
                "-n",
                "6",
                "127.0.0.1",
                ">nul",
                "&",
                "exit",
                "0",
            ],
        )
    } else {
        ("/bin/sh", vec!["-c", "sleep 5 & exit 0"])
    };
    let command = LaunchCommand {
        side: RunSide::Server,
        working_dir: project.join("run/server"),
        executable: executable.into(),
        args: args.into_iter().map(str::to_string).collect(),
    };

    let started = std::time::Instant::now();
    supervise_run(
        project,
        &command,
        &SystemProcessRunner,
        &NeverStop,
        &NeverConsole,
        std::time::Duration::from_secs(1),
        &mut |_| {},
    )
    .unwrap();

    assert!(
        started.elapsed() < std::time::Duration::from_secs(2),
        "the run waited for a process that inherited the output pipes"
    );
}

#[test]
fn system_process_runner_fixture() {
    std::thread::sleep(std::time::Duration::from_millis(20));
}

#[test]
fn system_process_runner_tracks_real_pid_exit_and_pending_sync() {
    let temp = tempfile::tempdir().unwrap();
    let project = temp.path();
    support::write_build_project(project);
    prepare_run_directory_with_api(project, &support::FixtureApi, RunSide::Client).unwrap();
    let run_dir = project.join("run/client");
    std::fs::write(run_dir.join("config/runtime.cfg"), "runtime").unwrap();
    let command = LaunchCommand {
        side: RunSide::Client,
        working_dir: run_dir,
        executable: std::env::current_exe().unwrap(),
        args: vec![
            "--exact".to_string(),
            "system_process_runner_fixture".to_string(),
            "--test-threads=1".to_string(),
        ],
    };

    let outcome = supervise_run(
        project,
        &command,
        &SystemProcessRunner,
        &NeverStop,
        &NeverConsole,
        std::time::Duration::from_secs(2),
        &mut |_| {},
    )
    .unwrap();

    assert!(outcome.pid > 0);
    assert!(outcome.status.success());
    assert_eq!(outcome.termination, RunTermination::Exited);
    assert_eq!(outcome.pending_sync.entries.len(), 1);
    assert!(outcome.pending_sync.path.is_file());
}

#[test]
fn system_process_runner_streams_stdout_and_stderr_before_nonzero_exit() {
    use std::time::{Duration, Instant};

    let temp = tempfile::tempdir().unwrap();
    let acknowledged = temp.path().join("output-observed");
    let finished = temp.path().join("process-finished");
    std::fs::write(temp.path().join("process-stream-fixture-enabled"), b"1").unwrap();
    let spec = ProcessSpec {
        executable: std::env::current_exe().unwrap(),
        args: vec![
            "--exact".to_string(),
            "system_process_runner_streaming_fixture".to_string(),
            "--nocapture".to_string(),
        ],
        working_dir: temp.path().to_path_buf(),
        suppress_console_window: true,
    };
    let mut process = SystemProcessRunner.spawn(&spec).unwrap();
    let mut output = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut observed_before_exit = false;
    let status = loop {
        process
            .drain_output(&mut |stream, line| {
                if line == "fixture stdout" || line == "fixture stderr" {
                    observed_before_exit |= !finished.is_file();
                    std::fs::write(&acknowledged, b"observed").unwrap();
                }
                output.push((stream, line));
            })
            .unwrap();
        if let Some(status) = process.try_wait().unwrap() {
            process
                .finish_output(&mut |stream, line| output.push((stream, line)))
                .unwrap();
            break status;
        }
        assert!(Instant::now() < deadline, "fixture did not exit in time");
        std::thread::sleep(Duration::from_millis(10));
    };

    assert_eq!(status.code(), Some(7));
    assert!(observed_before_exit);
    assert!(finished.is_file());
    assert!(output.iter().any(|(stream, line)| {
        *stream == ProcessOutputStream::Stdout && line == "fixture stdout"
    }));
    assert!(output.iter().any(|(stream, line)| {
        *stream == ProcessOutputStream::Stderr && line == "fixture stderr"
    }));
}

#[test]
fn system_process_runner_streaming_fixture() {
    let working_dir = std::env::current_dir().unwrap();
    if !working_dir.join("process-stream-fixture-enabled").is_file() {
        return;
    }
    use std::io::Write as _;

    println!("fixture stdout");
    std::io::stdout().flush().unwrap();
    eprintln!("fixture stderr");
    std::io::stderr().flush().unwrap();
    let acknowledged = working_dir.join("output-observed");
    let finished = working_dir.join("process-finished");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while !acknowledged.is_file() && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    std::fs::write(finished, b"finished").unwrap();
    std::process::exit(7);
}

struct EmptyJavaLocator;

impl JavaLocator for EmptyJavaLocator {
    fn path_java(&self, _major: u16) -> Option<std::path::PathBuf> {
        None
    }

    fn common_java(&self, _major: u16) -> Option<std::path::PathBuf> {
        None
    }
}

struct FixedJavaLocator {
    path: Option<std::path::PathBuf>,
    common: Option<std::path::PathBuf>,
}

impl JavaLocator for FixedJavaLocator {
    fn path_java(&self, _major: u16) -> Option<std::path::PathBuf> {
        self.path.clone()
    }

    fn common_java(&self, _major: u16) -> Option<std::path::PathBuf> {
        self.common.clone()
    }
}

fn test_java_executable_name() -> &'static str {
    if cfg!(windows) { "java.exe" } else { "java" }
}

fn test_zulu_platform_cache_key() -> String {
    let os = match std::env::consts::OS {
        "windows" => "windows",
        "linux" => "linux",
        "macos" => "macos",
        other => panic!("unsupported test OS {other}"),
    };
    let arch = match std::env::consts::ARCH {
        "x86_64" => "x64",
        "aarch64" => "aarch64",
        other => panic!("unsupported test architecture {other}"),
    };
    format!("{os}-{arch}")
}

fn zulu_fixture_archive(java_bytes: &[u8]) -> Vec<u8> {
    let mut archive = std::io::Cursor::new(Vec::new());
    {
        let mut zip = zip::ZipWriter::new(&mut archive);
        zip.start_file(
            format!("zulu-test/bin/{}", test_java_executable_name()),
            zip::write::SimpleFileOptions::default(),
        )
        .unwrap();
        std::io::Write::write_all(&mut zip, java_bytes).unwrap();
        zip.finish().unwrap();
    }
    archive.into_inner()
}

struct CountingDownloader {
    bytes: Vec<u8>,
    count: std::sync::atomic::AtomicUsize,
}

struct ZuluDownloader {
    metadata: Vec<u8>,
    archive: Vec<u8>,
    count: std::sync::atomic::AtomicUsize,
}

struct SlowZuluDownloader {
    metadata: Vec<u8>,
    archive: Vec<u8>,
    count: std::sync::atomic::AtomicUsize,
}

struct InterruptedZuluDownloader {
    metadata: Vec<u8>,
}

impl DownloadBytes for InterruptedZuluDownloader {
    fn download_bytes(&self, url: &str) -> Result<Vec<u8>, RunError> {
        if url.starts_with("https://api.azul.com/") {
            return Ok(self.metadata.clone());
        }
        Err(RunError::ZuluMetadata {
            reason: "interrupted fixture download".to_string(),
        })
    }
}

impl SlowZuluDownloader {
    fn count(&self) -> usize {
        self.count.load(std::sync::atomic::Ordering::SeqCst)
    }
}

impl DownloadBytes for SlowZuluDownloader {
    fn download_bytes(&self, url: &str) -> Result<Vec<u8>, RunError> {
        self.count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        std::thread::sleep(std::time::Duration::from_millis(100));
        if url.starts_with("https://api.azul.com/") {
            return Ok(self.metadata.clone());
        }
        if url == "https://example.invalid/zulu.zip" {
            return Ok(self.archive.clone());
        }
        panic!("unexpected Zulu fixture URL: {url}");
    }
}

impl ZuluDownloader {
    fn count(&self) -> usize {
        self.count.load(std::sync::atomic::Ordering::SeqCst)
    }
}

impl DownloadBytes for ZuluDownloader {
    fn download_bytes(&self, url: &str) -> Result<Vec<u8>, mcmpu::project::run::RunError> {
        self.count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        if url.starts_with("https://api.azul.com/") {
            return Ok(self.metadata.clone());
        }
        if url == "https://example.invalid/zulu.zip" {
            return Ok(self.archive.clone());
        }
        panic!("unexpected Zulu fixture URL: {url}");
    }
}

struct NoDownloadUrlApi;

impl CurseForgeApi for NoDownloadUrlApi {
    fn get_project(
        &self,
        project_id: u64,
    ) -> Result<mcmpu::curseforge::model::CfProject, ResolveError> {
        support::FixtureApi.get_project(project_id)
    }

    fn get_file(&self, project_id: u64, file_id: u64) -> Result<CfFile, ResolveError> {
        let mut file = support::FixtureApi.get_file(project_id, file_id)?;
        file.download_url = None;
        Ok(file)
    }

    fn find_project_by_slug(
        &self,
        slug: &str,
    ) -> Result<mcmpu::curseforge::model::CfProject, ResolveError> {
        support::FixtureApi.find_project_by_slug(slug)
    }

    fn list_files(
        &self,
        project_id: u64,
        minecraft_version: &str,
        loader: &str,
    ) -> Result<Vec<CfFile>, ResolveError> {
        let mut files = support::FixtureApi.list_files(project_id, minecraft_version, loader)?;
        for file in &mut files {
            file.download_url = None;
        }
        Ok(files)
    }

    fn list_project_files(&self, project_id: u64) -> Result<Vec<CfFile>, ResolveError> {
        let mut files = support::FixtureApi.list_project_files(project_id)?;
        for file in &mut files {
            file.download_url = None;
        }
        Ok(files)
    }
}

impl CountingDownloader {
    fn new(bytes: Vec<u8>) -> Self {
        Self {
            bytes,
            count: std::sync::atomic::AtomicUsize::new(0),
        }
    }

    fn count(&self) -> usize {
        self.count.load(std::sync::atomic::Ordering::SeqCst)
    }
}

impl DownloadBytes for CountingDownloader {
    fn download_bytes(&self, _url: &str) -> Result<Vec<u8>, mcmpu::project::run::RunError> {
        self.count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(self.bytes.clone())
    }
}

fn launch_plan(project: &std::path::Path, side: RunSide) -> mcmpu::project::run::RunLaunchPlan {
    let run_dir = project.join("run").join(side.dir_name());
    std::fs::create_dir_all(&run_dir).unwrap();
    mcmpu::project::run::RunLaunchPlan {
        side,
        run_dir,
        java_args: vec!["-Xmx4G".to_string()],
        player_name: "Player".to_string(),
        offline_uuid: offline_uuid_for_player("Player"),
        files: Vec::new(),
    }
}

fn launch_descriptor(side: LaunchSide) -> LaunchDescriptor {
    LaunchDescriptor {
        loader: LoaderKind::Forge,
        side,
        version_id: "test-version".to_string(),
        inheritance_chain: vec!["test-version".to_string()],
        main_class: "example.Main".to_string(),
        libraries: Vec::new(),
        classpath: vec!["libraries/example.jar".to_string()],
        client_jar: None,
        jvm_arguments: Vec::new(),
        game_arguments: Vec::new(),
        assets: Some("legacy".to_string()),
        asset_index: None,
        natives: Vec::new(),
        java_version_major: Some(8),
    }
}

struct AlwaysStop;

impl StopSignal for AlwaysStop {
    fn stop_requested(&self) -> bool {
        true
    }
}

#[derive(Default)]
struct FakeProcessState {
    try_wait_results: std::collections::VecDeque<Option<std::process::ExitStatus>>,
    wait_status: Option<std::process::ExitStatus>,
    stdin: Vec<u8>,
    terminate_count: usize,
}

struct FakeRunner {
    state: std::sync::Arc<std::sync::Mutex<FakeProcessState>>,
}

struct FailingRunner;

impl ProcessRunner for FailingRunner {
    fn spawn(&self, spec: &ProcessSpec) -> Result<Box<dyn RunningProcess>, ProcessError> {
        Err(ProcessError::Start {
            command: spec.display(),
            source: std::io::Error::new(std::io::ErrorKind::NotFound, "fixture spawn failure"),
        })
    }
}

impl FakeRunner {
    fn new(
        try_wait_results: Vec<Option<std::process::ExitStatus>>,
        wait_status: std::process::ExitStatus,
    ) -> Self {
        Self {
            state: std::sync::Arc::new(std::sync::Mutex::new(FakeProcessState {
                try_wait_results: try_wait_results.into(),
                wait_status: Some(wait_status),
                stdin: Vec::new(),
                terminate_count: 0,
            })),
        }
    }

    fn stdin_bytes(&self) -> Vec<u8> {
        self.state.lock().unwrap().stdin.clone()
    }

    fn terminate_count(&self) -> usize {
        self.state.lock().unwrap().terminate_count
    }
}

impl ProcessRunner for FakeRunner {
    fn spawn(&self, _spec: &ProcessSpec) -> Result<Box<dyn RunningProcess>, ProcessError> {
        Ok(Box::new(FakeRunningProcess {
            state: self.state.clone(),
        }))
    }
}

struct FakeRunningProcess {
    state: std::sync::Arc<std::sync::Mutex<FakeProcessState>>,
}

impl RunningProcess for FakeRunningProcess {
    fn pid(&self) -> u32 {
        4242
    }

    fn try_wait(&mut self) -> Result<Option<std::process::ExitStatus>, ProcessError> {
        Ok(self
            .state
            .lock()
            .unwrap()
            .try_wait_results
            .pop_front()
            .flatten())
    }

    fn wait(&mut self) -> Result<std::process::ExitStatus, ProcessError> {
        Ok(self.state.lock().unwrap().wait_status.unwrap())
    }

    fn write_stdin(&mut self, bytes: &[u8]) -> Result<(), ProcessError> {
        self.state.lock().unwrap().stdin.extend_from_slice(bytes);
        Ok(())
    }

    fn terminate(&mut self) -> Result<(), ProcessError> {
        self.state.lock().unwrap().terminate_count += 1;
        Ok(())
    }
}

#[cfg(unix)]
fn exit_status(code: i32) -> std::process::ExitStatus {
    use std::os::unix::process::ExitStatusExt;
    std::process::ExitStatus::from_raw(code << 8)
}

#[cfg(windows)]
fn exit_status(code: i32) -> std::process::ExitStatus {
    use std::os::windows::process::ExitStatusExt;
    std::process::ExitStatus::from_raw(code as u32)
}

fn file_with_name(id: u64, project_id: u64, file_name: &str) -> CfFile {
    CfFile {
        id,
        mod_id: project_id,
        file_name: file_name.to_string(),
        display_name: Some(file_name.to_string()),
        file_length: 42,
        is_available: true,
        file_status: 4,
        release_type: ReleaseType::Stable,
        game_versions: ["Forge", "1.12.2", "Client", "Server"]
            .into_iter()
            .map(ToOwned::to_owned)
            .collect(),
        dependencies: Vec::new(),
        download_url: Some("https://example.invalid/file.jar".to_string()),
        hashes: vec![FileHash {
            value: "0123456789abcdef0123456789abcdef01234567".to_string(),
            algo: 1,
        }],
    }
}
