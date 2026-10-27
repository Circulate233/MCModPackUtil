use std::collections::HashMap;
use std::io::Read;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Condvar, Mutex};
use std::time::Duration;

use mcmpu::config::{CleanroomRelauncher, ConfigError, LoaderKind};
use mcmpu::curseforge::model::{CfFile, CfProject};
use mcmpu::curseforge::resolver::{CurseForgeApi, ResolveError};
use mcmpu::dependency::{DependencyEntry, DependencySide, DependencyType};
use mcmpu::project::build::{
    BuildError, BuildOptions, BuildOutputs, BuildProgress, BuildTarget, ServerPlatform,
    ServerPlatformChoice, build_project_with_api_and_options,
    build_project_with_api_and_options_and_progress, build_project_with_api_key_and_options,
    build_project_with_api_key_and_options_and_progress, build_project_with_options,
    build_project_with_options_and_sources,
    build_project_with_sources as build_project_with_all_sources,
    build_project_with_sources_and_progress as build_project_with_all_sources_and_progress,
    build_project_with_sources_to, validate_project_with_api,
};
use mcmpu::project::cleanroom_relauncher::MmcPackSource;
use sha1::{Digest, Sha1};
use zip::ZipArchive;

mod support;

fn client_only_options() -> BuildOptions {
    BuildOptions {
        target: BuildTarget::Client,
        server_platform: ServerPlatformChoice::Current,
        output_dir_override: None,
    }
}

fn write_incomplete_configured_proxy(project: &std::path::Path) {
    support::write_build_project(project);
    let mut config = support::sample_config();
    config.network.proxy_port = 7890;
    std::fs::write(
        project.join("config.toml"),
        toml::to_string_pretty(&config).unwrap(),
    )
    .unwrap();
}

#[test]
fn build_entry_without_proxy_uses_configured_network_proxy() {
    let temp = tempfile::tempdir().unwrap();
    write_incomplete_configured_proxy(temp.path());

    let error = build_project_with_options(temp.path(), client_only_options()).unwrap_err();

    assert!(matches!(
        error,
        BuildError::Config(ConfigError::IncompleteProxy)
    ));
}

#[test]
fn injected_api_build_entries_use_configured_network_proxy() {
    let temp = tempfile::tempdir().unwrap();
    write_incomplete_configured_proxy(temp.path());

    let error = build_project_with_api_and_options(
        temp.path(),
        &support::FixtureApi,
        client_only_options(),
    )
    .unwrap_err();
    assert!(matches!(
        error,
        BuildError::Config(ConfigError::IncompleteProxy)
    ));

    let mut events = Vec::new();
    let error = build_project_with_api_and_options_and_progress(
        temp.path(),
        &support::FixtureApi,
        client_only_options(),
        &mut |event| events.push(event.clone()),
    )
    .unwrap_err();
    assert!(matches!(
        error,
        BuildError::Config(ConfigError::IncompleteProxy)
    ));
    assert!(events.is_empty());
}

#[test]
fn api_key_build_entries_without_explicit_proxy_use_configured_network_proxy() {
    let temp = tempfile::tempdir().unwrap();
    write_incomplete_configured_proxy(temp.path());

    let error = build_project_with_api_key_and_options(
        temp.path(),
        "test-api-key".to_string(),
        None,
        client_only_options(),
    )
    .unwrap_err();
    assert!(matches!(
        error,
        BuildError::Config(ConfigError::IncompleteProxy)
    ));

    let mut events = Vec::new();
    let error = build_project_with_api_key_and_options_and_progress(
        temp.path(),
        "test-api-key".to_string(),
        None,
        client_only_options(),
        &mut |event| events.push(event.clone()),
    )
    .unwrap_err();
    assert!(matches!(
        error,
        BuildError::Config(ConfigError::IncompleteProxy)
    ));
    assert!(events.is_empty());
}

#[test]
fn explicit_proxy_build_entry_does_not_use_configured_proxy() {
    let temp = tempfile::tempdir().unwrap();
    let project = temp.path();
    write_incomplete_configured_proxy(project);
    let mut config = support::sample_config();
    config.network.proxy_port = 7890;
    config.pack.loader = LoaderKind::Fabric;
    config.pack.loader_version = "0.16.14".to_string();
    config.pack.forge_compat_version = None;
    config.cleanroom = None;
    std::fs::write(
        project.join("config.toml"),
        toml::to_string_pretty(&config).unwrap(),
    )
    .unwrap();
    for list in [
        "common.json",
        "client.json",
        "server.json",
        "resourcepacks.json",
    ] {
        support::write_entries(project.join(list), &[]);
    }

    let artifacts = build_project_with_api_key_and_options(
        project,
        "test-api-key".to_string(),
        Some("http://127.0.0.1:9"),
        client_only_options(),
    )
    .unwrap();

    assert!(artifacts.client_zip.unwrap().is_file());
    assert!(artifacts.server_zip.is_none());

    let mut events = Vec::new();
    let artifacts = build_project_with_api_key_and_options_and_progress(
        project,
        "test-api-key".to_string(),
        Some("http://127.0.0.1:9"),
        client_only_options(),
        &mut |event| events.push(event.clone()),
    )
    .unwrap();
    assert!(artifacts.client_zip.unwrap().is_file());
    assert!(artifacts.server_zip.is_none());
    assert!(matches!(
        events.last(),
        Some(BuildProgress::Finished { .. })
    ));
}

fn build_project_with_sources<A, S>(
    project_dir: impl AsRef<std::path::Path>,
    api: &A,
    mmc_source: &S,
) -> Result<BuildOutputs, BuildError>
where
    A: CurseForgeApi + Sync,
    S: MmcPackSource,
{
    build_project_with_all_sources(
        project_dir,
        api,
        mmc_source,
        &support::FixtureServerInstallerSource,
    )
}

fn build_project_with_sources_and_progress<A, S, F>(
    project_dir: impl AsRef<std::path::Path>,
    api: &A,
    mmc_source: &S,
    progress: &mut F,
) -> Result<BuildOutputs, BuildError>
where
    A: CurseForgeApi + Sync,
    S: MmcPackSource,
    F: FnMut(&BuildProgress) + Send,
{
    build_project_with_all_sources_and_progress(
        project_dir,
        api,
        mmc_source,
        &support::FixtureServerInstallerSource,
        progress,
    )
}

#[test]
fn config_rejects_unsafe_output_and_portable_pack_file_names() {
    for output in ["../dist", "/tmp/dist", r"C:\dist", "safe/../../escape"] {
        let mut config = support::sample_config();
        config.build.output_dir = output.to_string();
        assert!(
            config.validate().is_err(),
            "accepted output directory {output}"
        );
    }
    for name in [
        "../pack",
        "folder/pack",
        r"folder\pack",
        "pack:stream",
        "pack.",
        "pack ",
        "CON",
        "COM1.zip",
        "LPT¹",
    ] {
        let mut config = support::sample_config();
        config.pack.name = name.to_string();
        assert!(config.validate().is_err(), "accepted pack name {name}");
    }
}

#[test]
fn explicit_build_output_may_be_outside_project() {
    let project_root = tempfile::tempdir().unwrap();
    let output_root = tempfile::tempdir().unwrap();
    support::write_build_project(project_root.path());

    let outputs = build_project_with_sources_to(
        project_root.path(),
        &support::FixtureApi,
        &support::FixtureMmcPackSource,
        &support::FixtureServerInstallerSource,
        Some(output_root.path()),
    )
    .unwrap();

    assert_eq!(outputs.client_zip.parent(), Some(output_root.path()));
    assert_eq!(outputs.server_zip.parent(), Some(output_root.path()));
    assert!(outputs.client_zip.is_file());
    assert!(outputs.server_zip.is_file());
    assert!(!project_root.path().join("dist").exists());
}

#[test]
fn build_rejects_runtime_and_generated_server_identities_in_overrides() {
    for relative in ["runtime/session.dat", "server-install.json"] {
        let temp = tempfile::tempdir().unwrap();
        support::write_build_project(temp.path());
        let path = temp.path().join("overrides/common").join(relative);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"reserved").unwrap();

        let error = build_project_with_sources(
            temp.path(),
            &support::FixtureApi,
            &support::FixtureMmcPackSource,
        )
        .unwrap_err();

        assert!(
            error
                .to_string()
                .contains(relative.split('/').next().unwrap())
        );
    }
}

#[cfg(unix)]
#[test]
fn build_rejects_override_symlink_before_writing_outputs() {
    use std::os::unix::fs::symlink;

    let temp = tempfile::tempdir().unwrap();
    support::write_build_project(temp.path());
    let outside = temp.path().join("outside.cfg");
    std::fs::write(&outside, b"outside").unwrap();
    symlink(
        &outside,
        temp.path().join("overrides/common/config/linked.cfg"),
    )
    .unwrap();

    let error = build_project_with_sources(
        temp.path(),
        &support::FixtureApi,
        &support::FixtureMmcPackSource,
    )
    .unwrap_err();

    assert!(error.to_string().contains("symbolic link"));
    assert!(!temp.path().join("dist").exists());
}

#[cfg(unix)]
#[test]
fn configured_output_rejects_symlink_escape_but_explicit_output_allows_external_path() {
    use std::os::unix::fs::symlink;

    let project = tempfile::tempdir().unwrap();
    let external = tempfile::tempdir().unwrap();
    support::write_build_project(project.path());
    symlink(external.path(), project.path().join("dist")).unwrap();

    let error = build_project_with_sources(
        project.path(),
        &support::FixtureApi,
        &support::FixtureMmcPackSource,
    )
    .unwrap_err();
    assert!(matches!(error, BuildError::InvalidOutputDirectory { .. }));

    let outputs = build_project_with_sources_to(
        project.path(),
        &support::FixtureApi,
        &support::FixtureMmcPackSource,
        &support::FixtureServerInstallerSource,
        Some(external.path()),
    )
    .unwrap();
    assert_eq!(outputs.client_zip.parent(), Some(external.path()));
}

#[derive(Default)]
struct RecordingMmcPackSource {
    requests: Mutex<Vec<String>>,
}

impl MmcPackSource for RecordingMmcPackSource {
    type Error = std::io::Error;

    fn fetch(&self, url: &str) -> Result<Vec<u8>, Self::Error> {
        self.requests.lock().unwrap().push(url.to_string());
        support::FixtureMmcPackSource.fetch(url)
    }
}

struct StaticMmcPackSource {
    bytes: Vec<u8>,
    sha1: Option<String>,
}

#[test]
fn build_rejects_server_memory_override_after_splitting_before_writing_outputs() {
    let temp = tempfile::tempdir().unwrap();
    let project = temp.path();
    support::write_build_project(project);
    let mut config = support::sample_config();
    config.java.server.extra_args = vec!["-Dserver=true -Xmx12G".to_string()];
    std::fs::write(
        project.join("config.toml"),
        toml::to_string_pretty(&config).unwrap(),
    )
    .unwrap();

    let error = build_project_with_sources(
        project,
        &support::FixtureApi,
        &support::FixtureMmcPackSource,
    )
    .unwrap_err();

    assert!(matches!(
        error,
        BuildError::JvmArguments(
            mcmpu::project::jvm_args::JvmArgumentError::MemoryOverride { ref argument }
        ) if argument == "-Xmx12G"
    ));
    assert!(!project.join("dist").exists());
}

impl MmcPackSource for StaticMmcPackSource {
    type Error = std::io::Error;

    fn fetch(&self, url: &str) -> Result<Vec<u8>, Self::Error> {
        if url.ends_with(".sha1") {
            return Ok(self
                .sha1
                .clone()
                .unwrap_or_else(|| format!("{:x}", Sha1::digest(&self.bytes)))
                .into_bytes());
        }
        Ok(self.bytes.clone())
    }
}

#[test]
fn build_generates_relauncher_config_for_each_resolved_cleanroom_variant() {
    for variant in [
        CleanroomRelauncher::Original,
        CleanroomRelauncher::Improved,
        CleanroomRelauncher::Unofficial,
    ] {
        let temp = tempfile::tempdir().unwrap();
        let project = temp.path();
        support::write_build_project(project);
        let mut config = support::sample_config();
        config.pack.loader_version = "0.6.2-alpha".to_string();
        config.java.client.min_memory = "3G".to_string();
        config.java.client.max_memory = "7G".to_string();
        config.java.client.extra_args = vec!["-XX:+UseG1GC".to_string()];
        config.cleanroom.as_mut().unwrap().relauncher = variant;
        std::fs::write(
            project.join("config.toml"),
            toml::to_string_pretty(&config).unwrap(),
        )
        .unwrap();

        let mmc_source = RecordingMmcPackSource::default();
        let outputs =
            build_project_with_sources(project, &support::FixtureApi, &mmc_source).unwrap();
        let mut client = open_zip(&outputs.client_zip);
        let mut server = open_zip(&outputs.server_zip);

        match variant {
            CleanroomRelauncher::Original => {
                let json = read_zip_json(&mut client, "overrides/config/relauncher.json");
                assert_eq!(json["selectedVersion"], "0.6.2-alpha");
                assert_eq!(json["latestVersion"], "0.6.2-alpha");
                assert_eq!(json["targetJavaVersion"], 25);
                assert_eq!(json["targetVendor"], "ZULU");
                assert_eq!(json["args"], "-Xms3G -Xmx7G -XX:+UseG1GC");
                assert_eq!(json["autoSetup"], true);
                assert_eq!(json["enableRelauncher"], true);
                assert_eq!(json["FetchUpdates"], false);
                assert_eq!(json["clearCleanroomFolder"], false);
                assert_eq!(json["clearJavaProvisionFolder"], false);
                assert!(!zip_has(
                    &mut client,
                    "overrides/cleanroom/cleanroom-0.6.2-alpha.zip"
                ));
            }
            CleanroomRelauncher::Improved => {
                let json = read_zip_json(&mut client, "overrides/config/relauncher.json");
                assert_eq!(json["selectedVersion"], "0.6.2-alpha");
                assert_eq!(json["latestVersion"], "0.6.2-alpha");
                assert_eq!(json["javaVendor"], "adoptium");
                assert_eq!(json["javaVersion"], 25);
                assert_eq!(json["darkMode"], true);
                assert_eq!(json["autoUpdate"], false);
                assert_eq!(json["disableCacheTTL"], false);
                assert_eq!(json["javaPath"], serde_json::Value::Null);
                assert_eq!(json["args"], "-Xms3G -Xmx7G -XX:+UseG1GC");
                assert!(!zip_has(
                    &mut client,
                    "overrides/cleanroom/cleanroom-0.6.2-alpha.zip"
                ));
            }
            CleanroomRelauncher::Unofficial => {
                let cfg = read_zip_text(&mut client, "overrides/config/cleanroom_relauncher.cfg");
                assert_eq!(cfg.lines().filter(|line| line.contains('=')).count(), 15);
                assert!(cfg.contains("B:\"Show Config\"=true"));
                assert!(cfg.contains("B:\"Always Show Config\"=false"));
                assert!(cfg.contains("B:\"Use Local MMC Pack\"=true"));
                assert!(
                    cfg.contains("S:\"Local MMC Pack Path\"=cleanroom/cleanroom-0.6.2-alpha.zip")
                );
                assert!(cfg.contains("B:\"Disable Update Toast\"=true"));
                assert!(cfg.contains("S:\"JVM Arguments\"=-Xms3G -Xmx7G -XX:+UseG1GC"));
                assert!(!cfg.contains("selectedVersion"));
                let mut pack = client
                    .by_name("overrides/cleanroom/cleanroom-0.6.2-alpha.zip")
                    .unwrap();
                assert_eq!(pack.compression(), zip::CompressionMethod::Stored);
                let mut packed_bytes = Vec::new();
                pack.read_to_end(&mut packed_bytes).unwrap();
                assert_eq!(packed_bytes, support::fixture_mmc_pack("0.6.2-alpha"));
            }
        }

        let requests = mmc_source.requests.lock().unwrap();
        if variant == CleanroomRelauncher::Unofficial {
            assert_eq!(requests.len(), 2);
            assert!(requests[0].ends_with("cleanroom-0.6.2-alpha.zip.sha1"));
            assert!(requests[1].ends_with("cleanroom-0.6.2-alpha.zip"));
        } else {
            assert!(requests.is_empty());
        }

        assert!(!zip_has(&mut server, "config/relauncher.json"));
        assert!(!zip_has(&mut server, "config/cleanroom_relauncher.cfg"));
    }
}

#[test]
fn build_rejects_existing_relauncher_config_in_common_or_client_overrides() {
    for (variant, relative_path) in [
        (CleanroomRelauncher::Original, "config/relauncher.json"),
        (
            CleanroomRelauncher::Unofficial,
            "config/cleanroom_relauncher.cfg",
        ),
        (
            CleanroomRelauncher::Unofficial,
            "cleanroom/cleanroom-1.0.0.zip",
        ),
    ] {
        for side in ["common", "client"] {
            let temp = tempfile::tempdir().unwrap();
            let project = temp.path();
            support::write_build_project(project);
            let mut config = support::sample_config();
            config.cleanroom.as_mut().unwrap().relauncher = variant;
            std::fs::write(
                project.join("config.toml"),
                toml::to_string_pretty(&config).unwrap(),
            )
            .unwrap();
            let path = project.join("overrides").join(side).join(relative_path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, "user-owned").unwrap();

            let error = build_project_with_sources(
                project,
                &support::FixtureApi,
                &support::FixtureMmcPackSource,
            )
            .unwrap_err();

            assert!(error.to_string().contains("already exists"));
            assert!(error.to_string().contains(side));
        }
    }
}

#[test]
fn build_reuses_verified_mmc_cache_and_repairs_corruption() {
    let temp = tempfile::tempdir().unwrap();
    let project = temp.path();
    support::write_build_project(project);
    let source = RecordingMmcPackSource::default();

    build_project_with_sources(project, &support::FixtureApi, &source).unwrap();
    build_project_with_sources(project, &support::FixtureApi, &source).unwrap();
    let cache_path = project.join(".mcmpu/cache/cleanroom/cleanroom-1.0.0.zip");
    assert_eq!(
        std::fs::read(&cache_path).unwrap(),
        support::fixture_mmc_pack("1.0.0")
    );
    {
        let requests = source.requests.lock().unwrap();
        assert_eq!(
            requests
                .iter()
                .filter(|url| !url.ends_with(".sha1"))
                .count(),
            1
        );
        assert_eq!(
            requests.iter().filter(|url| url.ends_with(".sha1")).count(),
            2
        );
    }

    std::fs::write(&cache_path, b"corrupt cache").unwrap();
    build_project_with_sources(project, &support::FixtureApi, &source).unwrap();

    assert_eq!(
        std::fs::read(cache_path).unwrap(),
        support::fixture_mmc_pack("1.0.0")
    );
    let requests = source.requests.lock().unwrap();
    assert_eq!(
        requests
            .iter()
            .filter(|url| !url.ends_with(".sha1"))
            .count(),
        2
    );
    assert_eq!(
        requests.iter().filter(|url| url.ends_with(".sha1")).count(),
        3
    );
}

#[test]
fn build_rejects_bad_mmc_sha1_structure_and_version() {
    let cases = [
        (
            StaticMmcPackSource {
                bytes: support::fixture_mmc_pack("1.0.0"),
                sha1: Some("0000000000000000000000000000000000000000".to_string()),
            },
            "SHA1 mismatch",
        ),
        (
            StaticMmcPackSource {
                bytes: support::fixture_mmc_pack_without_lwjgl("1.0.0"),
                sha1: None,
            },
            "patches/org.lwjgl3.json",
        ),
        (
            StaticMmcPackSource {
                bytes: support::fixture_mmc_pack("0.6.2-alpha"),
                sha1: None,
            },
            "component version",
        ),
    ];
    for (source, expected_error) in cases {
        let temp = tempfile::tempdir().unwrap();
        let project = temp.path();
        support::write_build_project(project);

        let error = build_project_with_sources(project, &support::FixtureApi, &source).unwrap_err();

        assert!(error.to_string().contains(expected_error));
        assert!(!project.join("dist/ExamplePack-cleanroom.zip").exists());
    }
}

#[test]
fn build_rejects_zero_or_multiple_resolved_relauncher_variants() {
    for entries in [
        vec![DependencyEntry {
            slug: Some("cleanroom-relauncher".to_string()),
            ..support::entry(999, 1999, DependencySide::Client)
        }],
        vec![
            support::entry(1_214_490, 2_214_490, DependencySide::Client),
            support::entry(1_336_201, 2_336_201, DependencySide::Client),
        ],
    ] {
        let temp = tempfile::tempdir().unwrap();
        let project = temp.path();
        support::write_build_project(project);
        support::write_entries(project.join("client.json"), &entries);

        let error = build_project_with_sources(
            project,
            &support::FixtureApi,
            &support::FixtureMmcPackSource,
        )
        .unwrap_err();

        assert!(error.to_string().contains("exactly one relauncher variant"));
    }
}

#[test]
fn build_writes_client_and_server_zip_outputs() {
    let temp = tempfile::tempdir().unwrap();
    let project = temp.path();
    support::write_build_project(project);

    let outputs = build_project_with_sources(
        project,
        &support::FixtureApi,
        &support::FixtureMmcPackSource,
    )
    .unwrap();

    assert_eq!(
        outputs.client_zip.file_name().unwrap().to_string_lossy(),
        "ExamplePack-cleanroom.zip"
    );
    assert_eq!(
        outputs.server_zip.file_name().unwrap().to_string_lossy(),
        format!(
            "ExamplePack-cleanroom-1.0.0-server-{}.zip",
            ServerPlatform::current().unwrap().as_str()
        )
    );

    let mut client = open_zip(&outputs.client_zip);
    assert!(zip_has(&mut client, "manifest.json"));
    assert!(zip_has(&mut client, "modlist.html"));
    assert!(zip_has(&mut client, "overrides/config/common.json"));
    assert!(zip_has(&mut client, "overrides/options.txt"));
    assert!(zip_has(
        &mut client,
        "overrides/config/cleanroom_relauncher.cfg"
    ));
    let relauncher_cfg = read_zip_text(&mut client, "overrides/config/cleanroom_relauncher.cfg");
    assert!(relauncher_cfg.contains("S:\"JVM Arguments\"=-Xms2G -Xmx4G"));
    assert!(relauncher_cfg.contains("B:\"Use Local MMC Pack\"=true"));
    assert!(relauncher_cfg.contains("S:\"Local MMC Pack Path\"=cleanroom/cleanroom-1.0.0.zip"));
    let mut nested_pack = client
        .by_name("overrides/cleanroom/cleanroom-1.0.0.zip")
        .unwrap();
    assert_eq!(nested_pack.compression(), zip::CompressionMethod::Stored);
    let mut nested_bytes = Vec::new();
    nested_pack.read_to_end(&mut nested_bytes).unwrap();
    assert_eq!(nested_bytes, support::fixture_mmc_pack("1.0.0"));
    drop(nested_pack);
    assert!(zip_has(&mut client, "README.md"));
    assert!(zip_has(&mut client, "LICENSE"));

    let manifest: serde_json::Value = read_zip_json(&mut client, "manifest.json");
    let files = manifest["files"].as_array().unwrap();
    assert!(files.iter().any(|file| file["projectID"] == 10));
    assert!(files.iter().any(|file| file["projectID"] == 20));
    assert!(files.iter().any(|file| file["projectID"] == 40));
    assert!(files.iter().any(|file| file["projectID"] == 50));
    assert!(files.iter().any(|file| file["projectID"] == 1_214_906));
    assert!(!files.iter().any(|file| file["projectID"] == 30));

    let mut server = open_zip(&outputs.server_zip);
    for removed in [
        "server-files.json",
        "start.bat",
        "start.sh",
        "mcmpu-download.ps1",
        "mcmpu-check-manual.ps1",
    ] {
        assert!(
            !zip_has(&mut server, removed),
            "legacy entry {removed} must not exist"
        );
    }
    assert!(zip_has(&mut server, "server-install.json"));
    assert!(zip_has(&mut server, "config/common.json"));
    assert!(zip_has(&mut server, "server.properties"));
    assert!(!zip_has(&mut server, "overrides/config/common.json"));
    assert!(!zip_has(&mut server, "overrides/server.properties"));
    assert!(!zip_has(&mut server, "config/relauncher.json"));
    assert!(!zip_has(&mut server, "config/cleanroom_relauncher.cfg"));
    assert!(!zip_has(&mut server, "cleanroom/cleanroom-1.0.0.zip"));
    assert!(zip_has(&mut server, "resourcepacks/.mcmpu-keep"));
    assert!(zip_has(&mut server, "README.md"));
    assert!(zip_has(&mut server, "LICENSE"));

    let platform = ServerPlatform::current().unwrap();
    let (name, expected, executable) = match platform {
        ServerPlatform::LinuxX86_64 => (
            "MCServerDownloadTool-linux-x86_64",
            b"fixture-linux-installer".as_slice(),
            true,
        ),
        ServerPlatform::MacosAarch64 => (
            "MCServerDownloadTool-macos-aarch64",
            b"fixture-macos-installer".as_slice(),
            true,
        ),
        ServerPlatform::WindowsX86_64 => (
            "MCServerDownloadTool-windows-x86_64.exe",
            b"fixture-windows-installer".as_slice(),
            false,
        ),
    };
    let mut asset = server.by_name(name).unwrap();
    assert_eq!(asset.compression(), zip::CompressionMethod::Stored);
    if executable {
        assert_eq!(asset.unix_mode().unwrap() & 0o777, 0o755);
    }
    let mut bytes = Vec::new();
    asset.read_to_end(&mut bytes).unwrap();
    assert_eq!(bytes, expected);
    drop(asset);
    for unexpected in [
        "MCServerDownloadTool-linux-x86_64",
        "MCServerDownloadTool-macos-aarch64",
        "MCServerDownloadTool-windows-x86_64.exe",
    ] {
        if unexpected != name {
            assert!(server.by_name(unexpected).is_err());
        }
    }

    let server_install: serde_json::Value = read_zip_json(&mut server, "server-install.json");
    assert_eq!(server_install["schema_version"], 1);
    assert_eq!(server_install["minecraft"]["version"], "1.12.2");
    assert_eq!(server_install["java"]["major"], 8);
    assert_eq!(server_install["java"]["min_memory_mb"], 2048);
    assert_eq!(server_install["java"]["max_memory_mb"], 4096);
    assert_eq!(
        server_install["java"]["jvm_args"],
        serde_json::json!(["-XX:+UseG1GC"])
    );
    assert_eq!(
        server_install["java"]["server_args"],
        serde_json::json!(["nogui"])
    );
    assert_eq!(server_install["loader"]["kind"], "cleanroom");
    assert_eq!(server_install["loader"]["version"], "1.0.0");
    assert_eq!(server_install["loader"]["output"]["type"], "exact_jar");
    assert_eq!(
        server_install["loader"]["output"]["path"],
        "cleanroom-1.0.0.jar"
    );
    assert_eq!(
        server_install["loader"]["output"]["main_class"],
        serde_json::Value::Null
    );
    assert_eq!(
        server_install["curseforge_api_key"],
        serde_json::json!("test-build-api-key")
    );

    let files = server_install["files"].as_array().unwrap();
    assert!(
        files
            .iter()
            .any(|file| file["path"] == "mods/common-mod.jar")
    );
    assert!(
        files
            .iter()
            .any(|file| file["path"] == "mods/server-mod.jar")
    );
    // A resource pack keeps the name it was uploaded with: the player's options reference it by
    // name and options.txt ships in this same archive.
    assert!(
        files
            .iter()
            .any(|file| file["path"] == "resourcepacks/resource-pack.zip"),
        "resource packs must keep their uploaded name: {files:#?}"
    );
    assert!(
        !files
            .iter()
            .any(|file| file["path"] == "mods/client-mod-20-1020.jar")
    );
    assert!(files.iter().all(|file| {
        let url = reqwest::Url::parse(file["download"]["url"].as_str().unwrap()).unwrap();
        file["download"]["mode"] == "automatic"
            && url.scheme() == "https"
            && url.host_str() == Some("edge.forgecdn.net")
            && url.port().is_none()
            && url.path().starts_with("/files/")
    }));
}

#[test]
fn build_archives_ignore_runtime_only_debug_layers() {
    let temp = tempfile::tempdir().unwrap();
    let project = temp.path();
    support::write_build_project(project);
    for (root, shared, unique) in [
        ("overrides/common", "release common", "common-only.cfg"),
        ("overrides/client", "release client", "client-only.cfg"),
        ("overrides/server", "release server", "server-only.cfg"),
    ] {
        std::fs::create_dir_all(project.join(root).join("config")).unwrap();
        std::fs::write(project.join(root).join("config/shared.cfg"), shared).unwrap();
        std::fs::write(project.join(root).join("config").join(unique), shared).unwrap();
    }
    for (root, shared, unique) in [
        ("debug/common", "debug common", "debug-common.cfg"),
        ("debug/client", "debug client", "debug-client.cfg"),
        ("debug/server", "debug server", "debug-server.cfg"),
    ] {
        std::fs::create_dir_all(project.join(root).join("config")).unwrap();
        std::fs::write(project.join(root).join("config/shared.cfg"), shared).unwrap();
        std::fs::write(project.join(root).join("config").join(unique), shared).unwrap();
    }

    let outputs = build_project_with_sources(
        project,
        &support::FixtureApi,
        &support::FixtureMmcPackSource,
    )
    .unwrap();

    let mut client = open_zip(&outputs.client_zip);
    assert_eq!(
        read_zip_text(&mut client, "overrides/config/shared.cfg"),
        "release client"
    );
    for name in [
        "overrides/config/debug-common.cfg",
        "overrides/config/debug-client.cfg",
        "overrides/config/debug-server.cfg",
    ] {
        assert!(!zip_has(&mut client, name));
    }

    let mut server = open_zip(&outputs.server_zip);
    assert_eq!(
        read_zip_text(&mut server, "config/shared.cfg"),
        "release server"
    );
    for name in [
        "config/debug-common.cfg",
        "config/debug-client.cfg",
        "config/debug-server.cfg",
    ] {
        assert!(!zip_has(&mut server, name));
    }
}

#[test]
fn build_resolves_slug_entries_without_requiring_download_urls() {
    let temp = tempfile::tempdir().unwrap();
    let project = temp.path();
    support::write_build_project(project);
    support::write_entries(
        project.join("common.json"),
        &[DependencyEntry {
            project_id: None,
            file_id: None,
            slug: Some("common-mod".to_string()),
            loader: None,
            side: DependencySide::Common,
            dependency_type: DependencyType::Mod,
            name: "Common Mod".to_string(),
            authors: Vec::new(),
            required: true,
        }],
    );

    let outputs = build_project_with_sources(
        project,
        &support::FixtureApi,
        &support::FixtureMmcPackSource,
    )
    .unwrap();
    let mut client = open_zip(&outputs.client_zip);
    let manifest: serde_json::Value = read_zip_json(&mut client, "manifest.json");

    assert!(
        manifest["files"]
            .as_array()
            .unwrap()
            .iter()
            .any(|file| file["projectID"] == 10 && file["fileID"] == 1010)
    );
}

#[test]
fn build_omits_loader_filter_when_dependency_loader_is_null() {
    let temp = tempfile::tempdir().unwrap();
    let project = temp.path();
    support::write_build_project(project);
    support::write_entries(
        project.join("common.json"),
        &[DependencyEntry {
            project_id: Some(10),
            file_id: None,
            slug: Some("common-mod".to_string()),
            loader: None,
            side: DependencySide::Common,
            dependency_type: DependencyType::Mod,
            name: "Common Mod".to_string(),
            authors: Vec::new(),
            required: true,
        }],
    );
    support::write_entries(project.join("client.json"), &[]);
    support::write_entries(project.join("server.json"), &[]);
    support::write_entries(project.join("resourcepacks.json"), &[]);
    let api = RecordingListFilesApi::default();

    build_project_with_sources(project, &api, &support::FixtureMmcPackSource).unwrap();

    assert!(
        api.calls
            .lock()
            .unwrap()
            .contains(&"10:1.12.2:".to_string())
    );
}

#[test]
fn build_server_files_use_curseforge_cdn_download_url() {
    let temp = tempfile::tempdir().unwrap();
    let project = temp.path();
    support::write_build_project(project);
    let outputs = build_project_with_sources(
        project,
        &support::FixtureApi,
        &support::FixtureMmcPackSource,
    )
    .unwrap();

    let mut server = open_zip(&outputs.server_zip);
    let server_files: serde_json::Value = read_zip_json(&mut server, "server-install.json");
    let files = server_files["files"].as_array().unwrap();

    assert!(files.iter().any(|file| {
        file["path"] == "mods/server-mod.jar"
            && file["name"] == "Server Mod"
            && file["type"] == "mod"
            && file["download"]["mode"] == "automatic"
            && file["download"]["url"] == "https://edge.forgecdn.net/files/30/1030/server-mod.jar"
            && file["project_page"]
                == "https://www.curseforge.com/minecraft/mc-mods/server-mod/files/1030"
    }));
}

#[test]
fn build_embeds_curseforge_key_only_for_forgecdn_automatic_downloads() {
    let temp = tempfile::tempdir().unwrap();
    let project = temp.path();
    support::write_build_project(project);

    let outputs = build_project_with_sources(
        project,
        &ServerDownloadUrlOverride(Some(
            "https://edge.forgecdn.net/files/1/2/server-mod.jar".to_string(),
        )),
        &support::FixtureMmcPackSource,
    )
    .unwrap();
    let mut server = open_zip(&outputs.server_zip);
    let manifest: serde_json::Value = read_zip_json(&mut server, "server-install.json");

    assert_eq!(manifest["curseforge_api_key"], "test-build-api-key");
    assert!(manifest["files"].as_array().unwrap().iter().any(|file| {
        file["path"] == "mods/server-mod.jar"
            && file["download"]["mode"] == "automatic"
            && file["download"]["url"] == "https://edge.forgecdn.net/files/1/2/server-mod.jar"
    }));
}

#[test]
fn build_server_marks_missing_curseforge_download_url_for_manual_install() {
    let temp = tempfile::tempdir().unwrap();
    let project = temp.path();
    support::write_build_project(project);
    let mut config = support::sample_config();
    config.pack.loader = mcmpu::config::LoaderKind::Forge;
    config.pack.loader_version = "14.23.5.2860".to_string();
    config.pack.forge_compat_version = None;
    config.cleanroom = None;
    std::fs::write(
        project.join("config.toml"),
        toml::to_string_pretty(&config).unwrap(),
    )
    .unwrap();
    support::write_entries(project.join("common.json"), &[]);
    support::write_entries(project.join("client.json"), &[]);
    support::write_entries(
        project.join("server.json"),
        &[support::entry(30, 1030, DependencySide::Server)],
    );
    support::write_entries(project.join("resourcepacks.json"), &[]);

    let outputs = build_project_with_sources(
        project,
        &ServerDownloadUrlOverride(None),
        &support::FixtureMmcPackSource,
    )
    .unwrap();
    let mut server = open_zip(&outputs.server_zip);
    let server_files: serde_json::Value = read_zip_json(&mut server, "server-install.json");
    let manual = server_files["files"]
        .as_array()
        .unwrap()
        .iter()
        .find(|file| file["path"] == "mods/server-mod.jar")
        .unwrap();

    assert_eq!(manual["name"], "Server Mod");
    assert_eq!(manual["type"], "mod");
    assert_eq!(manual["download"]["mode"], "manual");
    assert!(manual["download"].get("url").is_none());
    assert_eq!(manual["sha1"], "0123456789abcdef0123456789abcdef01234567");
    assert_eq!(manual["size"], 1234);
    assert_eq!(
        manual["project_page"],
        "https://www.curseforge.com/minecraft/mc-mods/server-mod/files/1030"
    );
    assert!(server_files.get("curseforge_api_key").is_none());
    assert!(zip_has(&mut server, "config/common.json"));
    assert!(!zip_has(&mut server, "overrides/config/common.json"));
}

#[test]
fn build_server_writes_one_entry_with_server_override_winning() {
    let temp = tempfile::tempdir().unwrap();
    let project = temp.path();
    support::write_build_project(project);
    std::fs::write(
        project.join("overrides/common/config/layered.cfg"),
        "common",
    )
    .unwrap();
    std::fs::create_dir_all(project.join("overrides/server/config")).unwrap();
    std::fs::write(
        project.join("overrides/server/config/layered.cfg"),
        "server",
    )
    .unwrap();

    let outputs = build_project_with_sources(
        project,
        &support::FixtureApi,
        &support::FixtureMmcPackSource,
    )
    .unwrap();
    let mut server = open_zip(&outputs.server_zip);

    assert_eq!(read_zip_text(&mut server, "config/layered.cfg"), "server");
    assert_eq!(
        (0..server.len())
            .filter(|index| server.by_index(*index).unwrap().name() == "config/layered.cfg")
            .count(),
        1
    );
}

#[test]
fn build_server_rejects_installer_reserved_override_paths() {
    let temp = tempfile::tempdir().unwrap();
    let project = temp.path();
    support::write_build_project(project);
    std::fs::write(project.join("overrides/server/start.bat"), "user script").unwrap();

    let error = build_project_with_sources(
        project,
        &support::FixtureApi,
        &support::FixtureMmcPackSource,
    )
    .unwrap_err();

    assert!(matches!(
        error,
        BuildError::ServerOverrideLayout { ref reason }
            if reason.contains("installer-reserved") && reason.contains("start.bat")
    ));
}

#[test]
fn build_rejects_duplicate_server_manifest_targets_before_writing_zip() {
    let temp = tempfile::tempdir().unwrap();
    let project = temp.path();
    support::write_build_project(project);
    support::write_entries(
        project.join("server.json"),
        &[support::entry(10, 1010, DependencySide::Server)],
    );

    let error = build_project_with_sources(
        project,
        &support::FixtureApi,
        &support::FixtureMmcPackSource,
    )
    .unwrap_err();

    assert!(matches!(
        error,
        BuildError::ServerManifest(
            mcmpu::server::files::ServerManifestError::DuplicateTarget { .. }
        )
    ));
}

#[test]
fn build_server_treats_blank_curseforge_download_url_as_manual() {
    let temp = tempfile::tempdir().unwrap();
    let project = temp.path();
    support::write_build_project(project);

    let outputs = build_project_with_sources(
        project,
        &ServerDownloadUrlOverride(Some("   ".to_string())),
        &support::FixtureMmcPackSource,
    )
    .unwrap();
    let mut server = open_zip(&outputs.server_zip);
    let server_files: serde_json::Value = read_zip_json(&mut server, "server-install.json");
    let manual = server_files["files"]
        .as_array()
        .unwrap()
        .iter()
        .find(|file| file["path"] == "mods/server-mod.jar")
        .unwrap();

    assert_eq!(manual["download"]["mode"], "manual");
    assert_eq!(manual["path"], "mods/server-mod.jar");
}

#[test]
fn build_server_treats_unavailable_curseforge_file_as_manual() {
    let temp = tempfile::tempdir().unwrap();
    let project = temp.path();
    support::write_build_project(project);

    let outputs = build_project_with_sources(
        project,
        &ServerAvailabilityOverride(false),
        &support::FixtureMmcPackSource,
    )
    .unwrap();
    let mut server = open_zip(&outputs.server_zip);
    let server_files: serde_json::Value = read_zip_json(&mut server, "server-install.json");
    let manual = server_files["files"]
        .as_array()
        .unwrap()
        .iter()
        .find(|file| file["path"] == "mods/server-mod.jar")
        .unwrap();

    assert_eq!(manual["download"]["mode"], "manual");
    assert!(manual["download"].get("url").is_none());
}

#[test]
fn build_fails_instead_of_overwriting_user_relauncher_config() {
    let temp = tempfile::tempdir().unwrap();
    let project = temp.path();
    support::write_build_project(project);
    let config_dir = project.join("overrides/client/config");
    std::fs::create_dir_all(&config_dir).unwrap();
    std::fs::write(
        config_dir.join("cleanroom_relauncher.cfg"),
        "general {\n}\n",
    )
    .unwrap();

    let err = build_project_with_sources(
        project,
        &support::FixtureApi,
        &support::FixtureMmcPackSource,
    )
    .unwrap_err();

    assert!(err.to_string().contains("already exists"));
    assert!(err.to_string().contains("cleanroom_relauncher.cfg"));
}

#[test]
fn build_does_not_block_on_curseforge_required_dependency_metadata() {
    let temp = tempfile::tempdir().unwrap();
    let project = temp.path();
    support::write_build_project(project);

    let outputs = build_project_with_sources(
        project,
        &RequiredDependencyApi,
        &support::FixtureMmcPackSource,
    )
    .unwrap();

    assert!(outputs.client_zip.exists());
    assert!(outputs.server_zip.exists());
}

#[test]
fn build_keeps_invalid_json_override_files_without_failing() {
    let temp = tempfile::tempdir().unwrap();
    let project = temp.path();
    support::write_build_project(project);
    std::fs::write(
        project.join("overrides/common/config/not-really-json.json"),
        "this mod uses json as txt",
    )
    .unwrap();

    let outputs = build_project_with_sources(
        project,
        &support::FixtureApi,
        &support::FixtureMmcPackSource,
    )
    .unwrap();

    let mut client = open_zip(&outputs.client_zip);
    assert_eq!(
        read_zip_text(&mut client, "overrides/config/not-really-json.json"),
        "this mod uses json as txt"
    );
}

#[test]
fn build_uses_dependency_list_bucket_when_curseforge_side_metadata_is_missing() {
    let temp = tempfile::tempdir().unwrap();
    let project = temp.path();
    support::write_build_project(project);
    support::write_entries(
        project.join("common.json"),
        &[DependencyEntry {
            project_id: Some(60),
            file_id: Some(1060),
            slug: Some("unknown-side-mod".to_string()),
            loader: None,
            side: DependencySide::Common,
            dependency_type: DependencyType::Mod,
            name: "Unknown Side Mod".to_string(),
            authors: Vec::new(),
            required: true,
        }],
    );

    let outputs = build_project_with_sources(
        project,
        &support::FixtureApi,
        &support::FixtureMmcPackSource,
    )
    .unwrap();
    let mut client = open_zip(&outputs.client_zip);
    let manifest: serde_json::Value = read_zip_json(&mut client, "manifest.json");
    let mut server = open_zip(&outputs.server_zip);
    let server_files: serde_json::Value = read_zip_json(&mut server, "server-install.json");

    assert!(
        manifest["files"]
            .as_array()
            .unwrap()
            .iter()
            .any(|file| file["projectID"] == 60 && file["fileID"] == 1060)
    );
    assert!(
        server_files["files"]
            .as_array()
            .unwrap()
            .iter()
            .any(|file| file["path"] == "mods/unknown-side-mod.jar")
    );
}

#[test]
fn build_reports_progress_while_resolving_and_writing_outputs() {
    let temp = tempfile::tempdir().unwrap();
    let project = temp.path();
    support::write_build_project(project);
    let mut events = Vec::new();

    build_project_with_sources_and_progress(
        project,
        &support::FixtureApi,
        &support::FixtureMmcPackSource,
        &mut |event| events.push(event.clone()),
    )
    .unwrap();

    assert!(matches!(
        events.first(),
        Some(BuildProgress::ReadingProject { .. })
    ));
    assert!(events.iter().any(|event| matches!(
        event,
        BuildProgress::ResolvingDependencies { total } if *total > 0
    )));
    assert!(events.iter().any(|event| matches!(
        event,
        BuildProgress::ResolvedDependency { completed, total, .. } if completed == total
    )));
    let preparing_installer = events
        .iter()
        .position(|event| matches!(event, BuildProgress::PreparingServerInstallerAssets))
        .unwrap();
    let installer_ready = events
        .iter()
        .position(|event| matches!(event, BuildProgress::ServerInstallerAssetsReady))
        .unwrap();
    assert!(preparing_installer < installer_ready);
    let writing_client = events
        .iter()
        .position(|event| matches!(event, BuildProgress::WritingClientZip { .. }))
        .unwrap();
    let writing_server = events
        .iter()
        .position(|event| matches!(event, BuildProgress::WritingServerZip { .. }))
        .unwrap();
    assert!(preparing_installer < writing_client);
    assert!(writing_client < installer_ready);
    assert!(installer_ready < writing_server);
    assert!(events.iter().any(|event| {
        matches!(event, BuildProgress::WritingClientZip { path } if path.ends_with("ExamplePack-cleanroom.zip"))
    }));
    assert!(events.iter().any(|event| {
        matches!(
            event,
            BuildProgress::WritingServerZip { path } if path.contains("-server-")
        )
    }));
    assert!(matches!(
        events.last(),
        Some(BuildProgress::Finished {
            client_zip,
            server_zip
        }) if client_zip.ends_with("ExamplePack-cleanroom.zip") && server_zip.contains("-server-")
    ));
}

#[derive(Default)]
struct BlockingInstallerState {
    request_started: bool,
    payload_started: bool,
    payload_started_after_client_archive: bool,
    released: bool,
}

#[derive(Default)]
struct BlockingServerInstallerSource {
    state: Mutex<BlockingInstallerState>,
    changed: Condvar,
}

impl BlockingServerInstallerSource {
    fn wait_until_request_started(&self) {
        let state = self.state.lock().unwrap();
        let (state, timeout) = self
            .changed
            .wait_timeout_while(state, Duration::from_secs(10), |state| {
                !state.request_started
            })
            .unwrap();
        assert!(
            state.request_started,
            "server installer metadata request did not start"
        );
        assert!(!timeout.timed_out());
    }

    fn wait_until_payload_started(&self) {
        let state = self.state.lock().unwrap();
        let (state, timeout) = self
            .changed
            .wait_timeout_while(state, Duration::from_secs(10), |state| {
                !state.payload_started
            })
            .unwrap();
        assert!(
            state.payload_started,
            "server installer payload preparation did not start"
        );
        assert!(
            state.payload_started_after_client_archive,
            "installer payload preparation started before the client temporary archive completed"
        );
        assert!(!timeout.timed_out());
    }

    fn release(&self) {
        let mut state = self.state.lock().unwrap();
        state.released = true;
        self.changed.notify_all();
    }
}

impl mcmpu::server::installer::ServerInstallerSource for BlockingServerInstallerSource {
    type Error = std::io::Error;

    fn request(
        &self,
        platform: ServerPlatform,
        request_started: &mut dyn FnMut(),
    ) -> Result<
        Box<dyn mcmpu::server::installer::ServerInstallerRequest<Error = Self::Error> + '_>,
        Self::Error,
    > {
        {
            let mut state = self.state.lock().unwrap();
            state.request_started = true;
            self.changed.notify_all();
        }
        request_started();
        Ok(Box::new(BlockingServerInstallerRequest {
            source: self,
            platform,
        }))
    }

    fn prepare(
        &self,
        project_dir: &std::path::Path,
        platform: ServerPlatform,
    ) -> Result<mcmpu::server::installer::ServerInstallerAsset, Self::Error> {
        mcmpu::server::installer::ServerInstallerSource::prepare(
            &support::FixtureServerInstallerSource,
            project_dir,
            platform,
        )
    }
}

struct BlockingServerInstallerRequest<'a> {
    source: &'a BlockingServerInstallerSource,
    platform: ServerPlatform,
}

impl mcmpu::server::installer::ServerInstallerRequest for BlockingServerInstallerRequest<'_> {
    type Error = std::io::Error;

    fn prepare(
        self: Box<Self>,
        project_dir: &std::path::Path,
    ) -> Result<mcmpu::server::installer::ServerInstallerAsset, Self::Error> {
        let client_archive_completed = completed_client_temp(&project_dir.join("dist")).is_some();
        let mut state = self.source.state.lock().unwrap();
        state.payload_started = true;
        state.payload_started_after_client_archive = client_archive_completed;
        self.source.changed.notify_all();
        while !state.released {
            state = self.source.changed.wait(state).unwrap();
        }
        drop(state);
        mcmpu::server::installer::ServerInstallerSource::prepare(
            &support::FixtureServerInstallerSource,
            project_dir,
            self.platform,
        )
    }
}

fn completed_client_temp(output_dir: &std::path::Path) -> Option<std::path::PathBuf> {
    let entries = std::fs::read_dir(output_dir).ok()?;
    for entry in entries.flatten() {
        let path = entry.path();
        let name = path.file_name()?.to_string_lossy();
        if !name.contains(".ExamplePack-cleanroom.zip.mcmpu-client-") {
            continue;
        }
        let file = std::fs::File::open(&path).ok()?;
        let mut zip = ZipArchive::new(file).ok()?;
        if zip.by_name("manifest.json").is_ok() {
            return Some(path);
        }
    }
    None
}

fn client_temp_files(output_dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    std::fs::read_dir(output_dir)
        .unwrap()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .is_some_and(|name| name.to_string_lossy().contains(".mcmpu-client-"))
        })
        .collect()
}

#[test]
fn dual_build_requests_metadata_before_client_and_defers_payload_until_client_zip_is_complete() {
    let temp = tempfile::tempdir().unwrap();
    let project = temp.path();
    support::write_build_project(project);
    let installer = BlockingServerInstallerSource::default();
    let events = Mutex::new(Vec::new());

    std::thread::scope(|scope| {
        let build = scope.spawn(|| {
            build_project_with_all_sources_and_progress(
                project,
                &support::FixtureApi,
                &support::FixtureMmcPackSource,
                &installer,
                &mut |event| events.lock().unwrap().push(event.clone()),
            )
        });

        installer.wait_until_request_started();
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        let client_temp = loop {
            if let Some(path) = completed_client_temp(&project.join("dist")) {
                break path;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "client ZIP was not completed while installer preparation was blocked"
            );
            std::thread::sleep(Duration::from_millis(10));
        };
        let observed = events.lock().unwrap();
        assert!(
            observed
                .iter()
                .any(|event| matches!(event, BuildProgress::PreparingServerInstallerAssets))
        );
        assert!(
            observed
                .iter()
                .any(|event| matches!(event, BuildProgress::WritingClientZip { .. }))
        );
        assert!(
            !observed
                .iter()
                .any(|event| matches!(event, BuildProgress::ServerInstallerAssetsReady))
        );
        assert!(
            !observed
                .iter()
                .any(|event| matches!(event, BuildProgress::WritingServerZip { .. }))
        );
        drop(observed);

        installer.wait_until_payload_started();
        installer.release();
        let outputs = build.join().unwrap().unwrap();
        assert!(outputs.client_zip.is_file());
        assert!(outputs.server_zip.is_file());
        assert!(!client_temp.exists());
    });
}

struct PanickingServerInstallerSource;

impl mcmpu::server::installer::ServerInstallerSource for PanickingServerInstallerSource {
    type Error = std::io::Error;

    fn prepare(
        &self,
        _project_dir: &std::path::Path,
        _platform: ServerPlatform,
    ) -> Result<mcmpu::server::installer::ServerInstallerAsset, Self::Error> {
        panic!("intentional installer worker panic")
    }
}

#[test]
fn build_converts_server_installer_worker_panic_to_build_error() {
    let temp = tempfile::tempdir().unwrap();
    support::write_build_project(temp.path());

    let error = build_project_with_all_sources(
        temp.path(),
        &support::FixtureApi,
        &support::FixtureMmcPackSource,
        &PanickingServerInstallerSource,
    )
    .unwrap_err();

    assert!(matches!(
        error,
        BuildError::ServerInstaller(ref message)
            if message == "server installer payload preparation panicked"
    ));
    assert!(client_temp_files(&temp.path().join("dist")).is_empty());
    assert!(!temp.path().join("dist/ExamplePack-cleanroom.zip").exists());
    assert!(
        !temp
            .path()
            .join(format!(
                "dist/ExamplePack-cleanroom-1.0.0-server-{}.zip",
                ServerPlatform::current().unwrap().as_str()
            ))
            .exists()
    );
}

struct FailingServerInstallerSource;

impl mcmpu::server::installer::ServerInstallerSource for FailingServerInstallerSource {
    type Error = std::io::Error;

    fn prepare(
        &self,
        _project_dir: &std::path::Path,
        _platform: ServerPlatform,
    ) -> Result<mcmpu::server::installer::ServerInstallerAsset, Self::Error> {
        Err(std::io::Error::other("intentional installer failure"))
    }
}

#[test]
fn build_cleans_client_temp_when_server_installer_fails() {
    let temp = tempfile::tempdir().unwrap();
    support::write_build_project(temp.path());

    let error = build_project_with_all_sources(
        temp.path(),
        &support::FixtureApi,
        &support::FixtureMmcPackSource,
        &FailingServerInstallerSource,
    )
    .unwrap_err();

    assert!(matches!(
        error,
        BuildError::ServerInstaller(ref message) if message == "intentional installer failure"
    ));
    assert!(client_temp_files(&temp.path().join("dist")).is_empty());
}

struct ForbiddenMmcPackSource;

impl MmcPackSource for ForbiddenMmcPackSource {
    type Error = std::io::Error;

    fn fetch(&self, url: &str) -> Result<Vec<u8>, Self::Error> {
        Err(std::io::Error::other(format!(
            "client MMC source must not be used for server-only build: {url}"
        )))
    }
}

#[derive(Default)]
struct FailingShaderApi {
    shader_requests: AtomicUsize,
}

impl FailingShaderApi {
    fn reject_shader(&self, project_id: u64) -> Result<(), ResolveError> {
        if project_id == 50 {
            self.shader_requests.fetch_add(1, Ordering::SeqCst);
            return Err(ResolveError::Api(
                "shader project must not be resolved for a server-only build".to_string(),
            ));
        }
        Ok(())
    }
}

impl CurseForgeApi for FailingShaderApi {
    fn get_project(&self, project_id: u64) -> Result<CfProject, ResolveError> {
        self.reject_shader(project_id)?;
        support::FixtureApi.get_project(project_id)
    }

    fn get_file(&self, project_id: u64, file_id: u64) -> Result<CfFile, ResolveError> {
        self.reject_shader(project_id)?;
        support::FixtureApi.get_file(project_id, file_id)
    }

    fn find_project_by_slug(&self, slug: &str) -> Result<CfProject, ResolveError> {
        support::FixtureApi.find_project_by_slug(slug)
    }

    fn list_files(
        &self,
        project_id: u64,
        minecraft_version: &str,
        loader: &str,
    ) -> Result<Vec<CfFile>, ResolveError> {
        self.reject_shader(project_id)?;
        support::FixtureApi.list_files(project_id, minecraft_version, loader)
    }
}

#[test]
fn client_only_skips_server_lists_and_installer() {
    let temp = tempfile::tempdir().unwrap();
    support::write_build_project(temp.path());
    std::fs::write(temp.path().join("server.json"), "not JSON").unwrap();
    let artifacts = build_project_with_options_and_sources(
        temp.path(),
        &support::FixtureApi,
        &support::FixtureMmcPackSource,
        &PanickingServerInstallerSource,
        BuildOptions {
            target: BuildTarget::Client,
            server_platform: ServerPlatformChoice::Current,
            output_dir_override: None,
        },
    )
    .unwrap();

    assert!(artifacts.client_zip.unwrap().is_file());
    assert!(artifacts.server_zip.is_none());
}

#[test]
fn server_only_skips_client_lists_and_mmc_preparation() {
    let temp = tempfile::tempdir().unwrap();
    support::write_build_project(temp.path());
    std::fs::write(temp.path().join("client.json"), "not JSON").unwrap();
    let artifacts = build_project_with_options_and_sources(
        temp.path(),
        &support::FixtureApi,
        &ForbiddenMmcPackSource,
        &support::FixtureServerInstallerSource,
        BuildOptions {
            target: BuildTarget::Server,
            server_platform: ServerPlatformChoice::Explicit(ServerPlatform::WindowsX86_64),
            output_dir_override: None,
        },
    )
    .unwrap();

    assert!(artifacts.client_zip.is_none());
    let server_zip = artifacts.server_zip.unwrap();
    assert!(server_zip.is_file());
    assert!(server_zip.ends_with("ExamplePack-cleanroom-1.0.0-server-windows-x86_64.zip"));
    let mut archive = open_zip(&server_zip);
    assert!(zip_has(
        &mut archive,
        "MCServerDownloadTool-windows-x86_64.exe"
    ));
    assert!(!zip_has(&mut archive, "MCServerDownloadTool-linux-x86_64"));
    assert!(!zip_has(&mut archive, "MCServerDownloadTool-macos-aarch64"));
}

#[test]
fn server_only_skips_shader_resolution_while_client_still_resolves_shaders() {
    let server_project = tempfile::tempdir().unwrap();
    support::write_build_project(server_project.path());
    let api = FailingShaderApi::default();

    let artifacts = build_project_with_options_and_sources(
        server_project.path(),
        &api,
        &ForbiddenMmcPackSource,
        &support::FixtureServerInstallerSource,
        BuildOptions {
            target: BuildTarget::Server,
            server_platform: ServerPlatformChoice::Explicit(ServerPlatform::WindowsX86_64),
            output_dir_override: None,
        },
    )
    .unwrap();

    assert!(artifacts.server_zip.unwrap().is_file());
    assert_eq!(api.shader_requests.load(Ordering::SeqCst), 0);

    let client_project = tempfile::tempdir().unwrap();
    support::write_build_project(client_project.path());
    let error = build_project_with_options_and_sources(
        client_project.path(),
        &api,
        &support::FixtureMmcPackSource,
        &support::FixtureServerInstallerSource,
        client_only_options(),
    )
    .unwrap_err();

    assert!(matches!(
        error,
        BuildError::Resolve(ResolveError::Api(ref message))
            if message == "shader project must not be resolved for a server-only build"
    ));
    assert!(api.shader_requests.load(Ordering::SeqCst) > 0);
}

#[test]
fn client_only_rejects_explicit_server_platform() {
    let temp = tempfile::tempdir().unwrap();
    support::write_build_project(temp.path());
    let error = build_project_with_options_and_sources(
        temp.path(),
        &support::FixtureApi,
        &support::FixtureMmcPackSource,
        &support::FixtureServerInstallerSource,
        BuildOptions {
            target: BuildTarget::Client,
            server_platform: ServerPlatformChoice::Explicit(ServerPlatform::LinuxX86_64),
            output_dir_override: None,
        },
    )
    .unwrap_err();
    assert!(matches!(error, BuildError::InvalidOptions { .. }));
    assert!(!temp.path().join("dist").exists());
}

#[test]
fn build_reuses_curseforge_requests_across_dependency_lists() {
    let temp = tempfile::tempdir().unwrap();
    let project = temp.path();
    support::write_build_project(project);
    let duplicate = support::entry(10, 1010, DependencySide::Client);
    support::write_entries(project.join("client.json"), &[duplicate]);
    let api = CountingApi::default();

    validate_project_with_api(project, &api).unwrap();
    validate_project_with_api(project, &api).unwrap();

    assert_eq!(api.project_calls(10), 1);
    assert_eq!(api.file_calls(10, 1010), 1);
}

#[test]
fn build_resolves_entries_across_lists_concurrently() {
    if rayon::current_num_threads() <= 1 {
        return;
    }

    let temp = tempfile::tempdir().unwrap();
    let project = temp.path();
    support::write_build_project(project);
    support::write_entries(
        project.join("common.json"),
        &[support::entry(10, 1010, DependencySide::Common)],
    );
    support::write_entries(
        project.join("client.json"),
        &[support::entry(20, 1020, DependencySide::Client)],
    );
    support::write_entries(project.join("server.json"), &[]);
    support::write_entries(project.join("resourcepacks.json"), &[]);
    let api = ConcurrentApi::default();

    validate_project_with_api(project, &api).unwrap();

    assert!(api.max_in_flight.load(Ordering::SeqCst) > 1);
}

struct RequiredDependencyApi;

#[derive(Default)]
struct RecordingListFilesApi {
    calls: Mutex<Vec<String>>,
}

impl CurseForgeApi for RecordingListFilesApi {
    fn get_project(&self, project_id: u64) -> Result<CfProject, ResolveError> {
        support::FixtureApi.get_project(project_id)
    }

    fn get_file(&self, project_id: u64, file_id: u64) -> Result<CfFile, ResolveError> {
        support::FixtureApi.get_file(project_id, file_id)
    }

    fn find_project_by_slug(&self, slug: &str) -> Result<CfProject, ResolveError> {
        support::FixtureApi.find_project_by_slug(slug)
    }

    fn list_files(
        &self,
        project_id: u64,
        minecraft_version: &str,
        loader: &str,
    ) -> Result<Vec<CfFile>, ResolveError> {
        self.calls
            .lock()
            .unwrap()
            .push(format!("{project_id}:{minecraft_version}:{loader}"));
        support::FixtureApi.list_files(project_id, minecraft_version, loader)
    }

    fn list_project_files(&self, project_id: u64) -> Result<Vec<CfFile>, ResolveError> {
        support::FixtureApi.list_project_files(project_id)
    }
}

impl CurseForgeApi for RequiredDependencyApi {
    fn get_project(&self, project_id: u64) -> Result<CfProject, ResolveError> {
        support::FixtureApi.get_project(project_id)
    }

    fn get_file(&self, project_id: u64, file_id: u64) -> Result<CfFile, ResolveError> {
        let mut file = support::FixtureApi.get_file(project_id, file_id)?;
        if project_id == 10 {
            file.dependencies
                .push(mcmpu::curseforge::model::FileDependency {
                    mod_id: 999_999,
                    relation_type: 3,
                });
        }
        Ok(file)
    }

    fn find_project_by_slug(&self, slug: &str) -> Result<CfProject, ResolveError> {
        support::FixtureApi.find_project_by_slug(slug)
    }

    fn list_files(
        &self,
        project_id: u64,
        minecraft_version: &str,
        loader: &str,
    ) -> Result<Vec<CfFile>, ResolveError> {
        support::FixtureApi.list_files(project_id, minecraft_version, loader)
    }

    fn list_project_files(&self, project_id: u64) -> Result<Vec<CfFile>, ResolveError> {
        support::FixtureApi.list_project_files(project_id)
    }
}

struct ServerDownloadUrlOverride(Option<String>);

impl CurseForgeApi for ServerDownloadUrlOverride {
    fn get_project(&self, project_id: u64) -> Result<CfProject, ResolveError> {
        support::FixtureApi.get_project(project_id)
    }

    fn get_file(&self, project_id: u64, file_id: u64) -> Result<CfFile, ResolveError> {
        let mut file = support::FixtureApi.get_file(project_id, file_id)?;
        if project_id == 30 {
            file.download_url = self.0.clone();
        }
        Ok(file)
    }

    fn find_project_by_slug(&self, slug: &str) -> Result<CfProject, ResolveError> {
        support::FixtureApi.find_project_by_slug(slug)
    }

    fn list_files(
        &self,
        project_id: u64,
        minecraft_version: &str,
        loader: &str,
    ) -> Result<Vec<CfFile>, ResolveError> {
        support::FixtureApi.list_files(project_id, minecraft_version, loader)
    }

    fn list_project_files(&self, project_id: u64) -> Result<Vec<CfFile>, ResolveError> {
        support::FixtureApi.list_project_files(project_id)
    }
}

struct ServerAvailabilityOverride(bool);

impl CurseForgeApi for ServerAvailabilityOverride {
    fn get_project(&self, project_id: u64) -> Result<CfProject, ResolveError> {
        support::FixtureApi.get_project(project_id)
    }

    fn get_file(&self, project_id: u64, file_id: u64) -> Result<CfFile, ResolveError> {
        let mut file = support::FixtureApi.get_file(project_id, file_id)?;
        if project_id == 30 {
            file.is_available = self.0;
        }
        Ok(file)
    }

    fn find_project_by_slug(&self, slug: &str) -> Result<CfProject, ResolveError> {
        support::FixtureApi.find_project_by_slug(slug)
    }

    fn list_files(
        &self,
        project_id: u64,
        minecraft_version: &str,
        loader: &str,
    ) -> Result<Vec<CfFile>, ResolveError> {
        support::FixtureApi.list_files(project_id, minecraft_version, loader)
    }

    fn list_project_files(&self, project_id: u64) -> Result<Vec<CfFile>, ResolveError> {
        support::FixtureApi.list_project_files(project_id)
    }
}

fn open_zip(path: &std::path::Path) -> ZipArchive<std::fs::File> {
    ZipArchive::new(std::fs::File::open(path).unwrap()).unwrap()
}

fn zip_has(zip: &mut ZipArchive<std::fs::File>, name: &str) -> bool {
    zip.by_name(name).is_ok()
}

fn read_zip_json(zip: &mut ZipArchive<std::fs::File>, name: &str) -> serde_json::Value {
    serde_json::from_str(&read_zip_text(zip, name)).unwrap()
}

fn read_zip_text(zip: &mut ZipArchive<std::fs::File>, name: &str) -> String {
    let mut file = zip.by_name(name).unwrap();
    let mut text = String::new();
    file.read_to_string(&mut text).unwrap();
    text
}

#[derive(Default)]
struct CountingApi {
    project_calls: Mutex<HashMap<u64, usize>>,
    file_calls: Mutex<HashMap<(u64, u64), usize>>,
}

impl CountingApi {
    fn project_calls(&self, project_id: u64) -> usize {
        *self
            .project_calls
            .lock()
            .unwrap()
            .get(&project_id)
            .unwrap_or(&0)
    }

    fn file_calls(&self, project_id: u64, file_id: u64) -> usize {
        *self
            .file_calls
            .lock()
            .unwrap()
            .get(&(project_id, file_id))
            .unwrap_or(&0)
    }
}

impl CurseForgeApi for CountingApi {
    fn get_project(&self, project_id: u64) -> Result<CfProject, ResolveError> {
        *self
            .project_calls
            .lock()
            .unwrap()
            .entry(project_id)
            .or_default() += 1;
        support::FixtureApi.get_project(project_id)
    }

    fn get_file(&self, project_id: u64, file_id: u64) -> Result<CfFile, ResolveError> {
        *self
            .file_calls
            .lock()
            .unwrap()
            .entry((project_id, file_id))
            .or_default() += 1;
        support::FixtureApi.get_file(project_id, file_id)
    }

    fn find_project_by_slug(&self, slug: &str) -> Result<CfProject, ResolveError> {
        support::FixtureApi.find_project_by_slug(slug)
    }

    fn list_files(
        &self,
        project_id: u64,
        minecraft_version: &str,
        loader: &str,
    ) -> Result<Vec<CfFile>, ResolveError> {
        support::FixtureApi.list_files(project_id, minecraft_version, loader)
    }

    fn list_project_files(&self, project_id: u64) -> Result<Vec<CfFile>, ResolveError> {
        support::FixtureApi.list_project_files(project_id)
    }
}

#[derive(Default)]
struct ConcurrentApi {
    in_flight: AtomicUsize,
    max_in_flight: AtomicUsize,
}

impl ConcurrentApi {
    fn record_request_start(&self) -> RequestGuard<'_> {
        let current = self.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
        self.max_in_flight.fetch_max(current, Ordering::SeqCst);
        RequestGuard { api: self }
    }
}

struct RequestGuard<'a> {
    api: &'a ConcurrentApi,
}

impl Drop for RequestGuard<'_> {
    fn drop(&mut self) {
        self.api.in_flight.fetch_sub(1, Ordering::SeqCst);
    }
}

impl CurseForgeApi for ConcurrentApi {
    fn get_project(&self, project_id: u64) -> Result<CfProject, ResolveError> {
        let _guard = self.record_request_start();
        std::thread::sleep(Duration::from_millis(30));
        support::FixtureApi.get_project(project_id)
    }

    fn get_file(&self, project_id: u64, file_id: u64) -> Result<CfFile, ResolveError> {
        let _guard = self.record_request_start();
        std::thread::sleep(Duration::from_millis(30));
        support::FixtureApi.get_file(project_id, file_id)
    }

    fn find_project_by_slug(&self, slug: &str) -> Result<CfProject, ResolveError> {
        support::FixtureApi.find_project_by_slug(slug)
    }

    fn list_files(
        &self,
        project_id: u64,
        minecraft_version: &str,
        loader: &str,
    ) -> Result<Vec<CfFile>, ResolveError> {
        support::FixtureApi.list_files(project_id, minecraft_version, loader)
    }

    fn list_project_files(&self, project_id: u64) -> Result<Vec<CfFile>, ResolveError> {
        support::FixtureApi.list_project_files(project_id)
    }
}
