use std::path::PathBuf;

use mcmpu::gui::command::{
    AppLaunchMode, CommandForm, GuiCommand, GuiCommandError, GuiCommandTask, ProxyConfig,
    gui_launch_mode, launch_mode_from_arg_count,
};

#[test]
fn no_arguments_open_gui_and_arguments_use_cli() {
    assert_eq!(gui_launch_mode(["mcmpu"]), AppLaunchMode::Gui);
    assert_eq!(gui_launch_mode(["mcmpu", "--help"]), AppLaunchMode::Cli);
    assert_eq!(gui_launch_mode(["mcmpu", "build"]), AppLaunchMode::Cli);
    assert_eq!(launch_mode_from_arg_count(1), AppLaunchMode::Gui);
    assert_eq!(launch_mode_from_arg_count(2), AppLaunchMode::Cli);
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
        lang: String::new(),
        proxy: ProxyConfig::default(),
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
        lang: String::new(),
        proxy: ProxyConfig::default(),
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
        lang: String::new(),
        proxy: ProxyConfig::default(),
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
        lang: "zh-CN".to_string(),
        proxy: ProxyConfig {
            enabled: true,
            ip: "127.0.0.1".to_string(),
            port: "7890".to_string(),
        },
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
fn build_command_uses_current_dir_and_omits_empty_optional_args() {
    let form = CommandForm {
        command: GuiCommand::Build,
        use_custom_project_dir: false,
        project_dir: PathBuf::from("F:/project"),
        pack_zip: PathBuf::new(),
        use_custom_output_dir: false,
        output_dir: PathBuf::new(),
        lang: String::new(),
        proxy: ProxyConfig::default(),
    };

    let spec = form.to_command_spec().unwrap();

    assert_eq!(spec.current_dir, PathBuf::from("."));
    assert_eq!(spec.args, vec!["build"]);
    assert_eq!(spec.task, GuiCommandTask::Build);
    assert_eq!(spec.lang, None);
    assert_eq!(spec.proxy, None);
    assert!(spec.envs.is_empty());
    assert_eq!(spec.display_command(), "mcmpu build");
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
        lang: String::new(),
        proxy: ProxyConfig::default(),
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
            lang: "en-US".to_string(),
            proxy: ProxyConfig::default(),
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
fn required_paths_are_validated() {
    let missing_project = CommandForm {
        command: GuiCommand::Build,
        use_custom_project_dir: true,
        project_dir: PathBuf::new(),
        pack_zip: PathBuf::new(),
        use_custom_output_dir: false,
        output_dir: PathBuf::new(),
        lang: String::new(),
        proxy: ProxyConfig::default(),
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
        lang: String::new(),
        proxy: ProxyConfig::default(),
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
        lang: String::new(),
        proxy: ProxyConfig::default(),
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
        lang: String::new(),
        proxy: ProxyConfig::default(),
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
        lang: String::new(),
        proxy: ProxyConfig {
            enabled: true,
            ip: String::new(),
            port: "7890".to_string(),
        },
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
    assert_eq!(spec.args, vec!["--proxy", "http://127.0.0.1:7890", "build"]);
}
