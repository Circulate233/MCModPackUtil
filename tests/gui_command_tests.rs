use std::path::PathBuf;

use mcmpu::config::{DownloadSource, TestRunConfig};
use mcmpu::gui::command::{
    AppLaunchMode, CommandForm, GuiCommand, GuiCommandError, GuiCommandTask, ProxyConfig,
    gui_launch_mode, launch_mode_from_arg_count, normalize_build_options_for_project,
    normalize_build_server_platform,
};
use mcmpu::project::build::{BuildOptions, BuildTarget, ServerPlatform, ServerPlatformChoice};
use tempfile::tempdir;

#[test]
fn no_arguments_open_gui_and_arguments_use_cli() {
    assert_eq!(gui_launch_mode(["mcmpu"]), AppLaunchMode::Gui);
    assert_eq!(gui_launch_mode(["mcmpu", "--help"]), AppLaunchMode::Cli);
    assert_eq!(gui_launch_mode(["mcmpu", "build"]), AppLaunchMode::Cli);
    assert_eq!(launch_mode_from_arg_count(1), AppLaunchMode::Gui);
    assert_eq!(launch_mode_from_arg_count(2), AppLaunchMode::Cli);
}

#[test]
fn sidebar_commands_cover_every_page_without_a_performance_page() {
    assert_eq!(
        GuiCommand::all(),
        [
            GuiCommand::Init,
            GuiCommand::ModList,
            GuiCommand::Overrides,
            GuiCommand::Settings,
            GuiCommand::Build,
            GuiCommand::Check,
            GuiCommand::RunClient,
            GuiCommand::RunServer,
        ]
    );
}

#[test]
fn overrides_page_is_gui_only_and_has_no_cli_command_spec() {
    assert!(GuiCommand::all().contains(&GuiCommand::Overrides));

    let form = CommandForm {
        command: GuiCommand::Overrides,
        use_custom_project_dir: false,
        project_dir: PathBuf::new(),
        pack_zip: PathBuf::new(),
        use_custom_output_dir: false,
        output_dir: PathBuf::new(),
        build_target: BuildTarget::Both,
        build_server_platform: ServerPlatformChoice::Current,
        lang: String::new(),
        proxy: ProxyConfig::default(),
        test_run: TestRunConfig::default(),
        test_run_project_dir: None,
        client_sync_include_paths: String::new(),
        server_sync_include_paths: String::new(),
    };

    assert!(matches!(
        form.to_command_spec().unwrap_err(),
        GuiCommandError::GuiOnlyCommand
    ));
}

#[test]
fn mod_list_is_gui_only_and_has_no_cli_command_spec() {
    assert!(GuiCommand::all().contains(&GuiCommand::ModList));

    let form = CommandForm {
        command: GuiCommand::ModList,
        use_custom_project_dir: false,
        project_dir: PathBuf::new(),
        pack_zip: PathBuf::new(),
        use_custom_output_dir: false,
        output_dir: PathBuf::new(),
        build_target: BuildTarget::Both,
        build_server_platform: ServerPlatformChoice::Current,
        lang: String::new(),
        proxy: ProxyConfig::default(),
        test_run: TestRunConfig::default(),
        test_run_project_dir: None,
        client_sync_include_paths: String::new(),
        server_sync_include_paths: String::new(),
    };

    assert!(matches!(
        form.to_command_spec().unwrap_err(),
        GuiCommandError::GuiOnlyCommand
    ));
}

#[test]
fn sync_page_is_gui_only_and_has_no_cli_command_spec() {
    assert!(!GuiCommand::all().contains(&GuiCommand::Sync));

    let form = CommandForm {
        command: GuiCommand::Sync,
        use_custom_project_dir: false,
        project_dir: PathBuf::new(),
        pack_zip: PathBuf::new(),
        use_custom_output_dir: false,
        output_dir: PathBuf::new(),
        build_target: BuildTarget::Both,
        build_server_platform: ServerPlatformChoice::Current,
        lang: String::new(),
        proxy: ProxyConfig::default(),
        test_run: TestRunConfig::default(),
        test_run_project_dir: None,
        client_sync_include_paths: String::new(),
        server_sync_include_paths: String::new(),
    };

    assert!(matches!(
        form.to_command_spec().unwrap_err(),
        GuiCommandError::GuiOnlyCommand
    ));
}

#[test]
fn settings_page_is_gui_only_and_has_no_cli_command_spec() {
    assert!(GuiCommand::all().contains(&GuiCommand::Settings));

    let form = CommandForm {
        command: GuiCommand::Settings,
        use_custom_project_dir: false,
        project_dir: PathBuf::new(),
        pack_zip: PathBuf::new(),
        use_custom_output_dir: false,
        output_dir: PathBuf::new(),
        build_target: BuildTarget::Both,
        build_server_platform: ServerPlatformChoice::Current,
        lang: String::new(),
        proxy: ProxyConfig::default(),
        test_run: TestRunConfig::default(),
        test_run_project_dir: None,
        client_sync_include_paths: String::new(),
        server_sync_include_paths: String::new(),
    };

    assert!(matches!(
        form.to_command_spec().unwrap_err(),
        GuiCommandError::GuiOnlyCommand
    ));
}

#[test]
fn init_command_builds_cli_arguments_without_api_key_args() {
    let form = CommandForm {
        command: GuiCommand::Init,
        use_custom_project_dir: true,
        project_dir: PathBuf::from("F:/project"),
        pack_zip: PathBuf::from("F:/packs/pack.zip"),
        use_custom_output_dir: true,
        output_dir: PathBuf::from("F:/out"),
        build_target: BuildTarget::Both,
        build_server_platform: ServerPlatformChoice::Current,
        lang: "zh-CN".to_string(),
        proxy: ProxyConfig {
            enabled: true,
            ip: "127.0.0.1".to_string(),
            port: "7890".to_string(),
        },
        test_run: TestRunConfig::default(),
        test_run_project_dir: None,
        client_sync_include_paths: String::new(),
        server_sync_include_paths: String::new(),
    };

    let spec = form.to_command_spec().unwrap();

    assert_eq!(spec.current_dir, PathBuf::from("F:/project"));
    assert_eq!(
        spec.args,
        vec![
            "--lang",
            "zh-CN",
            "--proxy",
            "http://127.0.0.1:7890",
            "init",
            "F:/packs/pack.zip",
            "--output",
            "F:/out"
        ]
    );
    assert!(spec.envs.is_empty());
    assert_eq!(
        spec.task,
        GuiCommandTask::Init {
            zip: PathBuf::from("F:/packs/pack.zip"),
            output: PathBuf::from("F:/out"),
        }
    );
    assert_eq!(spec.lang.as_deref(), Some("zh-CN"));
    assert_eq!(spec.proxy.as_deref(), Some("http://127.0.0.1:7890"));
    assert_eq!(
        spec.display_command(),
        "mcmpu --lang zh-CN --proxy http://127.0.0.1:7890 init F:/packs/pack.zip --output F:/out"
    );
}

#[test]
fn build_command_defaults_to_both_targets_on_the_current_platform() {
    let form = CommandForm {
        command: GuiCommand::Build,
        use_custom_project_dir: false,
        project_dir: PathBuf::from("F:/project"),
        pack_zip: PathBuf::new(),
        use_custom_output_dir: false,
        output_dir: PathBuf::new(),
        build_target: BuildTarget::Both,
        build_server_platform: ServerPlatformChoice::Current,
        lang: String::new(),
        proxy: ProxyConfig::default(),
        test_run: TestRunConfig::default(),
        test_run_project_dir: None,
        client_sync_include_paths: String::new(),
        server_sync_include_paths: String::new(),
    };

    let spec = form.to_command_spec().unwrap();

    assert_eq!(spec.current_dir, PathBuf::from("."));
    assert_eq!(
        spec.args,
        vec!["build", "--target", "both", "--server-platform", "current"]
    );
    assert_eq!(
        spec.task,
        GuiCommandTask::Build {
            options: BuildOptions {
                target: BuildTarget::Both,
                server_platform: ServerPlatformChoice::Current,
                output_dir_override: None,
            },
        }
    );
    assert_eq!(spec.lang, None);
    assert_eq!(spec.proxy, None);
    assert!(spec.envs.is_empty());
    assert_eq!(
        spec.display_command(),
        "mcmpu build --target both --server-platform current"
    );
}

#[test]
fn build_command_carries_client_server_and_both_options() {
    let mut form = CommandForm {
        command: GuiCommand::Build,
        use_custom_project_dir: false,
        project_dir: PathBuf::new(),
        pack_zip: PathBuf::new(),
        use_custom_output_dir: false,
        output_dir: PathBuf::new(),
        build_target: BuildTarget::Client,
        build_server_platform: ServerPlatformChoice::Current,
        lang: String::new(),
        proxy: ProxyConfig::default(),
        test_run: TestRunConfig::default(),
        test_run_project_dir: None,
        client_sync_include_paths: String::new(),
        server_sync_include_paths: String::new(),
    };

    let client = form.to_command_spec().unwrap();
    assert_eq!(client.args, vec!["build", "--target", "client"]);
    assert_eq!(
        client.task,
        GuiCommandTask::Build {
            options: BuildOptions {
                target: BuildTarget::Client,
                server_platform: ServerPlatformChoice::Current,
                output_dir_override: None,
            },
        }
    );

    form.build_target = BuildTarget::Server;
    form.build_server_platform = ServerPlatformChoice::Explicit(ServerPlatform::WindowsX86_64);
    let server = form.to_command_spec().unwrap();
    assert_eq!(
        server.args,
        vec![
            "build",
            "--target",
            "server",
            "--server-platform",
            "windows-x86_64"
        ]
    );
    assert_eq!(
        server.task,
        GuiCommandTask::Build {
            options: BuildOptions {
                target: BuildTarget::Server,
                server_platform: ServerPlatformChoice::Explicit(ServerPlatform::WindowsX86_64),
                output_dir_override: None,
            },
        }
    );

    form.build_target = BuildTarget::Both;
    form.build_server_platform = ServerPlatformChoice::Explicit(ServerPlatform::LinuxX86_64);
    form.use_custom_output_dir = true;
    form.output_dir = PathBuf::from("F:/out");
    let both = form.to_command_spec().unwrap();
    assert_eq!(
        both.args,
        vec![
            "build",
            "--target",
            "both",
            "--server-platform",
            "linux-x86_64",
            "--output",
            "F:/out"
        ]
    );
    assert_eq!(
        both.task,
        GuiCommandTask::Build {
            options: BuildOptions {
                target: BuildTarget::Both,
                server_platform: ServerPlatformChoice::Explicit(ServerPlatform::LinuxX86_64),
                output_dir_override: Some(PathBuf::from("F:/out")),
            },
        }
    );
}

#[test]
fn direct_gui_build_resolves_relative_output_from_custom_project_directory() {
    let project_root = tempdir().unwrap();
    let project_dir = project_root.path().join("example-pack");
    let form = CommandForm {
        command: GuiCommand::Build,
        use_custom_project_dir: true,
        project_dir: project_dir.clone(),
        use_custom_output_dir: true,
        output_dir: PathBuf::from("artifacts/releases"),
        build_target: BuildTarget::Client,
        ..CommandForm::default()
    };

    let spec = form.to_command_spec().unwrap();
    let GuiCommandTask::Build { options } = &spec.task else {
        panic!("build form did not produce a build task");
    };
    let normalized = normalize_build_options_for_project(&spec.current_dir, options);

    assert_eq!(spec.current_dir, project_dir);
    assert_eq!(
        normalized.output_dir_override,
        Some(project_dir.join("artifacts/releases"))
    );
    assert_eq!(normalized.target, BuildTarget::Client);
    assert_eq!(normalized.server_platform, ServerPlatformChoice::Current);
}

#[test]
fn direct_gui_build_preserves_absolute_output_override() {
    let workspace = tempdir().unwrap();
    let project_dir = workspace.path().join("projects/example-pack");
    let absolute_output = workspace.path().join("published/example-pack");
    let options = BuildOptions {
        target: BuildTarget::Server,
        server_platform: ServerPlatformChoice::Explicit(ServerPlatform::WindowsX86_64),
        output_dir_override: Some(absolute_output.clone()),
    };

    let normalized = normalize_build_options_for_project(&project_dir, &options);

    assert_eq!(normalized.output_dir_override, Some(absolute_output));
    assert_eq!(normalized.target, options.target);
    assert_eq!(normalized.server_platform, options.server_platform);
}

#[test]
fn client_only_build_rejects_an_explicit_server_platform() {
    let form = CommandForm {
        command: GuiCommand::Build,
        build_target: BuildTarget::Client,
        build_server_platform: ServerPlatformChoice::Explicit(ServerPlatform::MacosAarch64),
        ..CommandForm::default()
    };

    assert!(matches!(
        form.to_command_spec().unwrap_err(),
        GuiCommandError::ServerPlatformForClientBuild
    ));
}

#[test]
fn selecting_client_build_resets_a_previously_selected_server_platform() {
    for target in [BuildTarget::Both, BuildTarget::Server] {
        let retained = normalize_build_server_platform(
            target,
            ServerPlatformChoice::Explicit(ServerPlatform::LinuxX86_64),
        );
        assert_eq!(
            retained,
            ServerPlatformChoice::Explicit(ServerPlatform::LinuxX86_64)
        );
    }

    assert_eq!(
        normalize_build_server_platform(
            BuildTarget::Client,
            ServerPlatformChoice::Explicit(ServerPlatform::MacosAarch64),
        ),
        ServerPlatformChoice::Current
    );
}

#[test]
fn init_omits_output_argument_when_using_cli_default() {
    let form = CommandForm {
        command: GuiCommand::Init,
        use_custom_project_dir: false,
        project_dir: PathBuf::new(),
        pack_zip: PathBuf::from("F:/packs/pack.zip"),
        use_custom_output_dir: false,
        output_dir: PathBuf::from("F:/out"),
        build_target: BuildTarget::Both,
        build_server_platform: ServerPlatformChoice::Current,
        lang: String::new(),
        proxy: ProxyConfig::default(),
        test_run: TestRunConfig::default(),
        test_run_project_dir: None,
        client_sync_include_paths: String::new(),
        server_sync_include_paths: String::new(),
    };

    let spec = form.to_command_spec().unwrap();

    assert_eq!(spec.current_dir, PathBuf::from("."));
    assert_eq!(spec.args, vec!["init", "F:/packs/pack.zip"]);
}

#[test]
fn project_commands_generate_expected_subcommand_names() {
    for (command, expected) in [
        (GuiCommand::Check, "check"),
        (GuiCommand::RunClient, "runClient"),
        (GuiCommand::RunServer, "runServer"),
    ] {
        let form = CommandForm {
            command,
            use_custom_project_dir: true,
            project_dir: PathBuf::from("F:/project"),
            pack_zip: PathBuf::new(),
            use_custom_output_dir: false,
            output_dir: PathBuf::new(),
            build_target: BuildTarget::Both,
            build_server_platform: ServerPlatformChoice::Current,
            lang: "en-US".to_string(),
            proxy: ProxyConfig::default(),
            test_run: TestRunConfig::default(),
            test_run_project_dir: matches!(command, GuiCommand::RunClient | GuiCommand::RunServer)
                .then(|| PathBuf::from("F:/project")),
            client_sync_include_paths: String::new(),
            server_sync_include_paths: String::new(),
        };

        let spec = form.to_command_spec().unwrap();

        assert_eq!(spec.current_dir, PathBuf::from("F:/project"));
        assert_eq!(spec.args, vec!["--lang", "en-US", expected]);
        assert_eq!(
            spec.task,
            match command {
                GuiCommand::Check => GuiCommandTask::Check,
                GuiCommand::RunClient => GuiCommandTask::RunClient,
                GuiCommand::RunServer => GuiCommandTask::RunServer,
                _ => unreachable!(),
            }
        );
    }
}

#[test]
fn run_commands_generate_sync_configuration() {
    let mut form = CommandForm {
        command: GuiCommand::RunClient,
        use_custom_project_dir: false,
        project_dir: PathBuf::new(),
        pack_zip: PathBuf::new(),
        use_custom_output_dir: false,
        output_dir: PathBuf::new(),
        build_target: BuildTarget::Both,
        build_server_platform: ServerPlatformChoice::Current,
        lang: String::new(),
        proxy: ProxyConfig::default(),
        test_run: TestRunConfig::default(),
        test_run_project_dir: Some(PathBuf::from(".")),
        client_sync_include_paths: "config\nclient-options".to_string(),
        server_sync_include_paths: "config\nserver-properties".to_string(),
    };

    let client = form.to_command_spec().unwrap();
    assert_eq!(client.args, vec!["runClient"]);
    assert_eq!(
        client.run_sync_config.unwrap().include_paths,
        vec!["client-options", "config"]
    );

    form.command = GuiCommand::RunServer;
    let server = form.to_command_spec().unwrap();
    assert_eq!(server.args, vec!["runServer"]);
    assert_eq!(
        server.run_sync_config.unwrap().include_paths,
        vec!["config", "server-properties"]
    );
}

#[test]
fn required_paths_are_validated() {
    let missing_project = CommandForm {
        command: GuiCommand::Build,
        use_custom_project_dir: true,
        project_dir: PathBuf::new(),
        pack_zip: PathBuf::new(),
        use_custom_output_dir: false,
        output_dir: PathBuf::new(),
        build_target: BuildTarget::Both,
        build_server_platform: ServerPlatformChoice::Current,
        lang: String::new(),
        proxy: ProxyConfig::default(),
        test_run: TestRunConfig::default(),
        test_run_project_dir: None,
        client_sync_include_paths: String::new(),
        server_sync_include_paths: String::new(),
    };
    assert!(matches!(
        missing_project.to_command_spec().unwrap_err(),
        GuiCommandError::MissingProjectDir
    ));

    let missing_zip = CommandForm {
        command: GuiCommand::Init,
        use_custom_project_dir: false,
        project_dir: PathBuf::from("F:/project"),
        pack_zip: PathBuf::new(),
        use_custom_output_dir: true,
        output_dir: PathBuf::from("F:/out"),
        build_target: BuildTarget::Both,
        build_server_platform: ServerPlatformChoice::Current,
        lang: String::new(),
        proxy: ProxyConfig::default(),
        test_run: TestRunConfig::default(),
        test_run_project_dir: None,
        client_sync_include_paths: String::new(),
        server_sync_include_paths: String::new(),
    };
    assert!(matches!(
        missing_zip.to_command_spec().unwrap_err(),
        GuiCommandError::MissingPackZip
    ));

    let missing_output = CommandForm {
        command: GuiCommand::Init,
        use_custom_project_dir: false,
        project_dir: PathBuf::from("F:/project"),
        pack_zip: PathBuf::from("F:/packs/pack.zip"),
        use_custom_output_dir: true,
        output_dir: PathBuf::new(),
        build_target: BuildTarget::Both,
        build_server_platform: ServerPlatformChoice::Current,
        lang: String::new(),
        proxy: ProxyConfig::default(),
        test_run: TestRunConfig::default(),
        test_run_project_dir: None,
        client_sync_include_paths: String::new(),
        server_sync_include_paths: String::new(),
    };
    assert!(matches!(
        missing_output.to_command_spec().unwrap_err(),
        GuiCommandError::MissingOutputDir
    ));
}

#[test]
fn project_command_specs_do_not_include_api_key_arguments() {
    let form = CommandForm {
        command: GuiCommand::Check,
        use_custom_project_dir: false,
        project_dir: PathBuf::new(),
        pack_zip: PathBuf::new(),
        use_custom_output_dir: false,
        output_dir: PathBuf::new(),
        build_target: BuildTarget::Both,
        build_server_platform: ServerPlatformChoice::Current,
        lang: String::new(),
        proxy: ProxyConfig::default(),
        test_run: TestRunConfig::default(),
        test_run_project_dir: None,
        client_sync_include_paths: String::new(),
        server_sync_include_paths: String::new(),
    };

    let spec = form.to_command_spec().unwrap();

    assert_eq!(spec.args, vec!["check"]);
    assert_eq!(spec.display_command(), "mcmpu check");
}

#[test]
fn proxy_requires_ip_and_port_when_enabled() {
    let mut form = CommandForm {
        command: GuiCommand::Build,
        use_custom_project_dir: false,
        project_dir: PathBuf::new(),
        pack_zip: PathBuf::new(),
        use_custom_output_dir: false,
        output_dir: PathBuf::new(),
        build_target: BuildTarget::Both,
        build_server_platform: ServerPlatformChoice::Current,
        lang: String::new(),
        proxy: ProxyConfig {
            enabled: true,
            ip: String::new(),
            port: "7890".to_string(),
        },
        test_run: TestRunConfig::default(),
        test_run_project_dir: None,
        client_sync_include_paths: String::new(),
        server_sync_include_paths: String::new(),
    };
    assert!(matches!(
        form.to_command_spec().unwrap_err(),
        GuiCommandError::MissingProxyIp
    ));

    form.proxy.ip = "127.0.0.1".to_string();
    form.proxy.port.clear();
    assert!(matches!(
        form.to_command_spec().unwrap_err(),
        GuiCommandError::MissingProxyPort
    ));

    form.proxy.port = "7890".to_string();
    let spec = form.to_command_spec().unwrap();
    assert_eq!(
        spec.args,
        vec![
            "--proxy",
            "http://127.0.0.1:7890",
            "build",
            "--target",
            "both",
            "--server-platform",
            "current"
        ]
    );
}

#[test]
fn run_spec_carries_and_saves_test_run_and_side_sync_configuration() {
    let dir = tempdir().unwrap();
    let test_run = TestRunConfig {
        player_name: "DevPlayer".to_string(),
        download_source: DownloadSource::Bmclapi,
        sync_interval_seconds: 45,
        debug_extra_jvm_args: vec!["-Ddebug=true".to_string()],
        java_path: "F:/java/bin/java.exe".to_string(),
    };
    let form = CommandForm {
        command: GuiCommand::RunClient,
        use_custom_project_dir: true,
        project_dir: dir.path().to_path_buf(),
        pack_zip: PathBuf::new(),
        use_custom_output_dir: false,
        output_dir: PathBuf::new(),
        build_target: BuildTarget::Both,
        build_server_platform: ServerPlatformChoice::Current,
        lang: String::new(),
        proxy: ProxyConfig::default(),
        test_run: test_run.clone(),
        test_run_project_dir: Some(dir.path().to_path_buf()),
        client_sync_include_paths: "config\noptions".to_string(),
        server_sync_include_paths: "server-config".to_string(),
    };

    let spec = form.to_command_spec().unwrap();
    assert_eq!(spec.run_test_config.as_ref(), Some(&test_run));
    spec.save_run_configuration().unwrap();

    assert_eq!(TestRunConfig::from_project(dir.path()).unwrap(), test_run);
    let sync = mcmpu::project::run::sync_config_for_project(
        dir.path(),
        mcmpu::project::run::RunSide::Client,
    )
    .unwrap();
    assert_eq!(sync.include_paths, vec!["config", "options"]);
    assert!(!dir.path().join("run/server/.mcmpu-sync.toml").exists());
}

#[test]
fn run_client_rejects_empty_player_name_before_saving() {
    let mut form = CommandForm {
        command: GuiCommand::RunClient,
        test_run_project_dir: Some(PathBuf::from(".")),
        ..CommandForm::default()
    };
    form.test_run.player_name.clear();

    assert!(matches!(
        form.to_command_spec().unwrap_err(),
        GuiCommandError::MissingPlayerName
    ));
}

#[test]
fn run_spec_accepts_test_run_config_loaded_for_current_project() {
    let form = CommandForm {
        command: GuiCommand::RunServer,
        use_custom_project_dir: true,
        project_dir: PathBuf::from("F:/project-a"),
        test_run_project_dir: Some(PathBuf::from("F:/project-a")),
        ..CommandForm::default()
    };

    let spec = form.to_command_spec().unwrap();

    assert_eq!(
        spec.run_test_config_project_dir,
        Some(PathBuf::from("F:/project-a"))
    );
}

#[test]
fn run_spec_rejects_project_change_until_test_run_config_is_reloaded() {
    let form = CommandForm {
        command: GuiCommand::RunServer,
        use_custom_project_dir: true,
        project_dir: PathBuf::from("F:/project-b"),
        test_run_project_dir: Some(PathBuf::from("F:/project-a")),
        ..CommandForm::default()
    };

    assert!(matches!(
        form.to_command_spec().unwrap_err(),
        GuiCommandError::TestRunConfigProjectMismatch { loaded, current }
            if loaded == "F:/project-a" && current == "F:/project-b"
    ));
}
