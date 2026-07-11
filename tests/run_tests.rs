use mcmpu::config::{DownloadSource, TestRunConfig};
use mcmpu::curseforge::model::{CfFile, FileHash, ReleaseType};
use mcmpu::curseforge::resolver::{CurseForgeApi, ResolveError};
use mcmpu::project::run::{
    CurseMavenFile, DownloadBytes, JavaLocator, LaunchCommand, PendingSyncChange,
    PendingSyncManifest, ProcessRunner, RunSide, RunSourceKind, RunSourceManifest, SyncConfig,
    SyncTarget, apply_pending_sync, curse_maven_artifact_url, offline_uuid_for_player,
    ordered_download_sources, plan_launch_command, prepare_run_directory_with_api,
    prepare_run_directory_with_api_and_progress, resolve_java_executable, run_file_name,
    run_launch_command, save_zulu_java_download, scan_pending_sync, verify_or_download_run_files,
};

mod support;

#[test]
fn test_run_config_defaults_and_round_trips() {
    let config = TestRunConfig::default();

    assert_eq!(config.player_name, "Player");
    assert_eq!(config.download_source, DownloadSource::Auto);
    assert_eq!(config.java_version_major, 8);
    assert_eq!(config.sync_interval_seconds, 30);
    assert!(config.java_path.is_empty());
    assert!(config.debug_extra_jvm_args.is_empty());

    let text = toml::to_string_pretty(&config).unwrap();
    let parsed: TestRunConfig = toml::from_str(&text).unwrap();

    assert_eq!(parsed, config);
}

#[test]
fn test_run_config_rejects_invalid_download_source() {
    let err = toml::from_str::<TestRunConfig>(
        r#"
player_name = "Player"
download_source = "random"
java_version_major = 8
sync_interval_seconds = 30
debug_extra_jvm_args = []
java_path = ""
"#,
    )
    .unwrap_err();

    assert!(err.to_string().contains("download_source"));
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
fn curse_maven_url_and_file_name_preserve_sort_prefix() {
    let file = file_with_name(6147102, 873867, "!Red-Core-MC-1.8-1.12-0.6.jar");
    let artifact = CurseMavenFile::new("red-core", &file).unwrap();

    assert_eq!(
        curse_maven_artifact_url(&artifact),
        "https://www.cursemaven.com/curse/maven/red-core-873867/6147102/red-core-873867-6147102.jar"
    );
    assert_eq!(run_file_name(&artifact), "!red-core-873867-6147102.jar");
}

#[test]
fn curse_maven_file_name_keeps_plus_prefix() {
    let file = file_with_name(20, 10, "+++LoadFirst.jar");
    let artifact = CurseMavenFile::new("load-first", &file).unwrap();

    assert_eq!(run_file_name(&artifact), "+++load-first-10-20.jar");
}

#[test]
fn prepare_run_directory_rebuilds_generated_dirs_and_preserves_world_data() {
    let temp = tempfile::tempdir().unwrap();
    let project = temp.path();
    support::write_build_project(project);
    std::fs::create_dir_all(project.join("run/client/mods")).unwrap();
    std::fs::create_dir_all(project.join("run/client/saves/world1")).unwrap();
    std::fs::write(project.join("run/client/mods/stale.jar"), "stale").unwrap();
    std::fs::write(project.join("run/client/saves/world1/level.dat"), "world").unwrap();
    std::fs::create_dir_all(project.join("debug/common/config")).unwrap();
    std::fs::write(project.join("debug/common/config/debug-only.cfg"), "debug").unwrap();

    let plan =
        prepare_run_directory_with_api(project, &support::FixtureApi, RunSide::Client).unwrap();

    assert!(!project.join("run/client/mods/stale.jar").exists());
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
            && entry.target == SyncTarget::Client
    }));
}

#[test]
fn scan_pending_sync_ignores_debug_sources_and_generated_dirs() {
    let temp = tempfile::tempdir().unwrap();
    let project = temp.path();
    support::write_build_project(project);
    std::fs::create_dir_all(project.join("debug/common/config")).unwrap();
    std::fs::write(project.join("debug/common/config/debug.cfg"), "debug").unwrap();

    prepare_run_directory_with_api(project, &support::FixtureApi, RunSide::Client).unwrap();
    std::fs::write(project.join("run/client/config/debug.cfg"), "changed debug").unwrap();
    std::fs::write(project.join("run/client/mods/local.jar"), "generated").unwrap();

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
    assert_eq!(pending.ignored_debug, 1);
    assert_eq!(pending.ignored_generated, 0);
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
fn prepare_run_directory_uses_curse_maven_without_curseforge_download_url() {
    let temp = tempfile::tempdir().unwrap();
    let project = temp.path();
    support::write_build_project(project);

    let plan = prepare_run_directory_with_api(project, &NoDownloadUrlApi, RunSide::Client).unwrap();

    assert!(
        plan.files
            .iter()
            .all(|file| file.url.contains("cursemaven"))
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
fn resolve_java_executable_prefers_explicit_config_path() {
    let temp = tempfile::tempdir().unwrap();
    let java = temp.path().join("bin/java.exe");
    std::fs::create_dir_all(java.parent().unwrap()).unwrap();
    std::fs::write(&java, "").unwrap();
    let mut config = TestRunConfig::default();
    config.java_path = java.display().to_string();

    let resolved = resolve_java_executable(temp.path(), &config, &EmptyJavaLocator).unwrap();

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
    let mut config = TestRunConfig::default();
    config.java_version_major = 21;

    let err = resolve_java_executable(temp.path(), &config, &EmptyJavaLocator).unwrap_err();

    let message = err.to_string();
    assert!(message.contains("major 21"));
    assert!(message.contains("version 21"));
}

#[test]
fn save_zulu_java_download_uses_downloader_and_project_major_directory() {
    let temp = tempfile::tempdir().unwrap();
    let downloads = CountingDownloader::new(b"zulu archive".to_vec());

    let archive = save_zulu_java_download(
        temp.path(),
        17,
        "https://example.invalid/zulu.zip",
        &downloads,
    )
    .unwrap();

    assert_eq!(downloads.count(), 1);
    assert_eq!(archive, temp.path().join(".mcmpu/java/17/zulu.zip"));
    assert_eq!(std::fs::read(archive).unwrap(), b"zulu archive");
}

#[test]
fn plan_launch_command_includes_client_offline_identity() {
    let temp = tempfile::tempdir().unwrap();
    let plan = mcmpu::project::run::RunLaunchPlan {
        side: RunSide::Client,
        run_dir: temp.path().join("run/client"),
        java_args: vec!["-Xmx4G".to_string()],
        player_name: "Player".to_string(),
        offline_uuid: offline_uuid_for_player("Player"),
        files: Vec::new(),
    };

    let command = plan_launch_command(&plan, std::path::Path::new("java")).unwrap();

    assert!(matches!(
        command,
        LaunchCommand {
            side: RunSide::Client,
            ..
        }
    ));
    assert!(command.args.contains(&"--username".to_string()));
    assert!(command.args.contains(&"Player".to_string()));
    assert!(command.args.contains(&"--uuid".to_string()));
    assert!(command.args.contains(&offline_uuid_for_player("Player")));
    assert!(command.args.contains(&"--accessToken".to_string()));
    assert!(command.args.contains(&"0".to_string()));
}

#[test]
fn plan_launch_command_uses_server_jar_and_nogui_for_server() {
    let temp = tempfile::tempdir().unwrap();
    let plan = mcmpu::project::run::RunLaunchPlan {
        side: RunSide::Server,
        run_dir: temp.path().join("run/server"),
        java_args: vec!["-Xmx4G".to_string()],
        player_name: "Player".to_string(),
        offline_uuid: offline_uuid_for_player("Player"),
        files: Vec::new(),
    };

    let command = plan_launch_command(&plan, std::path::Path::new("java")).unwrap();

    assert_eq!(command.args, vec!["-Xmx4G", "-jar", "server.jar", "nogui"]);
}

#[test]
fn run_launch_command_invokes_runner_and_writes_pending_sync_without_changing_overrides() {
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
    let runner = RecordingRunner::default();

    let pending_path = run_launch_command(project, &command, &runner).unwrap();

    assert_eq!(runner.count(), 1);
    assert_eq!(
        pending_path,
        project.join(".mcmpu/pending-sync-client.json")
    );
    assert!(pending_path.is_file());
    assert!(!project.join("overrides/common/config/runtime.cfg").exists());
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

struct CountingDownloader {
    bytes: Vec<u8>,
    count: std::sync::atomic::AtomicUsize,
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

#[derive(Default)]
struct RecordingRunner {
    count: std::sync::atomic::AtomicUsize,
}

impl RecordingRunner {
    fn count(&self) -> usize {
        self.count.load(std::sync::atomic::Ordering::SeqCst)
    }
}

impl ProcessRunner for RecordingRunner {
    fn run(&self, _command: &LaunchCommand) -> Result<(), mcmpu::project::run::RunError> {
        self.count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(())
    }
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
