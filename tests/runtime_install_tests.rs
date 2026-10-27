use mcmpu::config::LoaderKind;
use mcmpu::project::runtime_install::{
    RuntimeInstallError, RuntimeInstallProgress, find_loader_profile,
    installer_jvm_proxy_arguments, server_jvm_arguments, server_launch_output_path,
};

#[test]
fn installer_proxy_arguments_are_empty_without_a_proxy() {
    assert_eq!(
        installer_jvm_proxy_arguments(None).unwrap(),
        Vec::<String>::new()
    );
    assert_eq!(
        installer_jvm_proxy_arguments(Some("   ")).unwrap(),
        Vec::<String>::new()
    );
}

#[test]
fn installer_proxy_arguments_use_http_host_and_port_for_both_protocols() {
    assert_eq!(
        installer_jvm_proxy_arguments(Some("http://127.0.0.1:7897")).unwrap(),
        [
            "-Dhttp.proxyHost=127.0.0.1",
            "-Dhttp.proxyPort=7897",
            "-Dhttps.proxyHost=127.0.0.1",
            "-Dhttps.proxyPort=7897",
        ]
    );
}

#[test]
fn installer_proxy_arguments_decode_https_credentials_without_exposing_them_in_progress() {
    let arguments = installer_jvm_proxy_arguments(Some(
        "https://build-user:correct%20horse%20battery%20staple@proxy.example:8443",
    ))
    .unwrap();

    assert_eq!(
        arguments,
        [
            "-Dhttp.proxyHost=proxy.example",
            "-Dhttp.proxyPort=8443",
            "-Dhttps.proxyHost=proxy.example",
            "-Dhttps.proxyPort=8443",
            "-Dhttp.proxyUser=build-user",
            "-Dhttps.proxyUser=build-user",
            "-Dhttp.proxyPassword=correct horse battery staple",
            "-Dhttps.proxyPassword=correct horse battery staple",
        ]
    );

    let progress_preview = format!(
        "{:?}",
        RuntimeInstallProgress::RunningInstaller {
            loader: LoaderKind::Forge,
        }
    );
    assert!(!progress_preview.contains("correct horse battery staple"));
    assert!(!progress_preview.contains("build-user"));
}

#[test]
fn installer_proxy_arguments_use_scheme_default_port_when_omitted() {
    assert_eq!(
        installer_jvm_proxy_arguments(Some("http://proxy.example")).unwrap(),
        [
            "-Dhttp.proxyHost=proxy.example",
            "-Dhttp.proxyPort=80",
            "-Dhttps.proxyHost=proxy.example",
            "-Dhttps.proxyPort=80",
        ]
    );
}

#[test]
fn installer_proxy_arguments_fail_fast_for_invalid_or_unsupported_urls_without_leaking_passwords() {
    for proxy in [
        "not a proxy url",
        "http://",
        "socks5://build-user:do-not-log@proxy.example:1080",
    ] {
        let error = installer_jvm_proxy_arguments(Some(proxy)).unwrap_err();

        assert!(matches!(
            error,
            RuntimeInstallError::InvalidInstallerProxy { .. }
        ));
        assert!(!error.to_string().contains("do-not-log"));
        assert!(!error.to_string().contains("build-user"));
    }
}

#[test]
fn server_launch_outputs_and_arguments_cover_all_supported_loaders() {
    let root = std::path::Path::new("run/server");
    let forge = server_launch_output_path(root, &LoaderKind::Forge, "1.20.1", "47.4.0", true);
    let neoforge =
        server_launch_output_path(root, &LoaderKind::NeoForge, "1.21.1", "21.1.190", false);
    let fabric = server_launch_output_path(root, &LoaderKind::Fabric, "1.20.1", "0.16.14", true);
    let cleanroom =
        server_launch_output_path(root, &LoaderKind::Cleanroom, "1.12.2", "0.3.16-alpha", true);

    assert!(forge.ends_with("libraries/net/minecraftforge/forge/1.20.1-47.4.0/win_args.txt"));
    assert!(neoforge.ends_with("libraries/net/neoforged/neoforge/21.1.190/unix_args.txt"));
    assert_eq!(fabric, root.join("fabric-server-launch.jar"));
    assert_eq!(cleanroom, root.join("cleanroom-0.3.16-alpha.jar"));

    // The JVM starts in the run directory, so every argument must be run-relative: reusing the
    // run-directory path verbatim made the JVM look for `run/server/run/server/...`.
    assert_eq!(
        server_jvm_arguments(&LoaderKind::Forge, root, &forge).unwrap(),
        vec!["@libraries/net/minecraftforge/forge/1.20.1-47.4.0/win_args.txt".to_string()]
    );
    assert_eq!(
        server_jvm_arguments(&LoaderKind::NeoForge, root, &neoforge).unwrap(),
        vec!["@libraries/net/neoforged/neoforge/21.1.190/unix_args.txt".to_string()]
    );
    assert_eq!(
        server_jvm_arguments(&LoaderKind::Fabric, root, &fabric).unwrap(),
        vec!["-jar".to_string(), "fabric-server-launch.jar".to_string()]
    );
    assert_eq!(
        server_jvm_arguments(&LoaderKind::Cleanroom, root, &cleanroom).unwrap(),
        vec!["-jar".to_string(), "cleanroom-0.3.16-alpha.jar".to_string()]
    );

    // A path outside the run directory cannot be executed by the server JVM.
    let outside = std::path::Path::new("dist/cleanroom-0.3.16-alpha.jar");
    assert!(matches!(
        server_jvm_arguments(&LoaderKind::Cleanroom, root, outside),
        Err(RuntimeInstallError::ServerLaunchOutputOutsideRunDirectory { .. })
    ));
}

#[test]
fn selects_installer_profile_only_when_json_declares_expected_parent() {
    let temp = tempfile::tempdir().unwrap();
    write_profile(
        temp.path(),
        "1.20.1-forge-47.4.0",
        "1.20.1-forge-47.4.0",
        Some("1.20.1"),
    );
    write_profile(
        temp.path(),
        "1.20.1-forge-other",
        "1.20.1-forge-other",
        Some("1.19.4"),
    );

    let selected =
        find_loader_profile(temp.path(), &LoaderKind::Forge, "1.20.1", "47.4.0").unwrap();

    assert_eq!(selected, "1.20.1-forge-47.4.0");
}

#[test]
fn rejects_ambiguous_installer_profiles() {
    let temp = tempfile::tempdir().unwrap();
    write_profile(
        temp.path(),
        "1.20.1-forge-47.4.0-a",
        "1.20.1-forge-47.4.0-a",
        Some("1.20.1"),
    );
    write_profile(
        temp.path(),
        "1.20.1-forge-47.4.0-b",
        "1.20.1-forge-47.4.0-b",
        Some("1.20.1"),
    );

    let error =
        find_loader_profile(temp.path(), &LoaderKind::Forge, "1.20.1", "47.4.0").unwrap_err();

    assert!(matches!(
        error,
        RuntimeInstallError::AmbiguousProfile { .. }
    ));
}

#[test]
fn rejects_installer_profile_with_directory_id_mismatch() {
    let temp = tempfile::tempdir().unwrap();
    write_profile(
        temp.path(),
        "1.20.1-forge-47.4.0",
        "different-profile-id",
        Some("1.20.1"),
    );

    let error =
        find_loader_profile(temp.path(), &LoaderKind::Forge, "1.20.1", "47.4.0").unwrap_err();

    assert!(matches!(error, RuntimeInstallError::InvalidProfile { .. }));
}

#[test]
fn selects_standalone_cleanroom_profile_with_real_installer_shape() {
    let temp = tempfile::tempdir().unwrap();
    write_profile_document(
        temp.path(),
        "1.12.2-Cleanroom-0.6.4-alpha",
        "1.12.2-Cleanroom-0.6.4-alpha",
        None,
        Some("top.outlands.foundation.boot.Foundation"),
    );

    let selected =
        find_loader_profile(temp.path(), &LoaderKind::Cleanroom, "1.12.2", "0.6.4-alpha").unwrap();

    assert_eq!(selected, "1.12.2-Cleanroom-0.6.4-alpha");
}

#[test]
fn inherited_loaders_reject_standalone_profiles() {
    for (loader, id, loader_version) in [
        (LoaderKind::Forge, "1.20.1-forge-47.4.0", "47.4.0"),
        (
            LoaderKind::Fabric,
            "fabric-loader-0.16.10-1.20.1",
            "0.16.10",
        ),
        (LoaderKind::NeoForge, "1.20.1-neoforge-20.1.120", "20.1.120"),
    ] {
        let temp = tempfile::tempdir().unwrap();
        write_profile_document(temp.path(), id, id, None, Some("fixture.Main"));

        let error =
            find_loader_profile(temp.path(), &loader, "1.20.1", loader_version).unwrap_err();

        assert!(matches!(error, RuntimeInstallError::InvalidProfile { .. }));
        assert!(error.to_string().contains("does not inherit"));
    }
}

#[test]
fn rejects_standalone_cleanroom_profile_without_cleanroom_identity() {
    let temp = tempfile::tempdir().unwrap();
    write_profile_document(
        temp.path(),
        "1.12.2-custom-0.6.4-alpha",
        "1.12.2-custom-0.6.4-alpha",
        None,
        Some("top.outlands.foundation.boot.Foundation"),
    );

    let error = find_loader_profile(temp.path(), &LoaderKind::Cleanroom, "1.12.2", "0.6.4-alpha")
        .unwrap_err();

    assert!(matches!(error, RuntimeInstallError::InvalidProfile { .. }));
    assert!(error.to_string().contains("must identify"));
}

#[test]
fn rejects_standalone_cleanroom_profile_without_main_class() {
    let temp = tempfile::tempdir().unwrap();
    write_profile_document(
        temp.path(),
        "1.12.2-Cleanroom-0.6.4-alpha",
        "1.12.2-Cleanroom-0.6.4-alpha",
        None,
        None,
    );

    let error = find_loader_profile(temp.path(), &LoaderKind::Cleanroom, "1.12.2", "0.6.4-alpha")
        .unwrap_err();

    assert!(matches!(error, RuntimeInstallError::InvalidProfile { .. }));
    assert!(error.to_string().contains("mainClass"));
}

fn write_profile(root: &std::path::Path, directory_id: &str, id: &str, parent: Option<&str>) {
    write_profile_document(root, directory_id, id, parent, None);
}

fn write_profile_document(
    root: &std::path::Path,
    directory_id: &str,
    id: &str,
    parent: Option<&str>,
    main_class: Option<&str>,
) {
    let directory = root.join("versions").join(directory_id);
    std::fs::create_dir_all(&directory).unwrap();
    let mut profile = serde_json::Map::new();
    profile.insert("id".to_string(), serde_json::Value::String(id.to_string()));
    if let Some(parent) = parent {
        profile.insert(
            "inheritsFrom".to_string(),
            serde_json::Value::String(parent.to_string()),
        );
    }
    if let Some(main_class) = main_class {
        profile.insert(
            "mainClass".to_string(),
            serde_json::Value::String(main_class.to_string()),
        );
    }
    std::fs::write(
        directory.join(format!("{directory_id}.json")),
        serde_json::to_vec(&profile).unwrap(),
    )
    .unwrap();
}
