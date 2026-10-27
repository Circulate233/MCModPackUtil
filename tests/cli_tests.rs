use assert_cmd::Command;
use clap::Parser;
use mcmpu::cli::{Cli, CliBuildTarget, CliServerPlatform, Command as CliCommand};
use predicates::prelude::*;

mod support;

#[test]
fn help_lists_check_command() {
    let mut command = Command::cargo_bin("mcmpu").unwrap();

    command
        .arg("--help")
        .assert()
        .success()
        .stdout(predicate::str::contains("check"))
        .stdout(predicate::str::contains("init"))
        .stdout(predicate::str::contains("build"));
}

#[test]
fn run_client_commands_parse_to_client_variant() {
    let camel = Cli::try_parse_from(["mcmpu", "runClient"]).unwrap();
    let alias = Cli::try_parse_from(["mcmpu", "run-client"]).unwrap();

    assert!(matches!(camel.command, CliCommand::RunClient));
    assert!(matches!(alias.command, CliCommand::RunClient));
}

#[test]
fn run_server_commands_parse_to_server_variant() {
    let camel = Cli::try_parse_from(["mcmpu", "runServer"]).unwrap();
    let alias = Cli::try_parse_from(["mcmpu", "run-server"]).unwrap();

    assert!(matches!(camel.command, CliCommand::RunServer));
    assert!(matches!(alias.command, CliCommand::RunServer));
}

#[test]
fn gui_command_parses_to_gui_variant() {
    let parsed = Cli::try_parse_from(["mcmpu", "gui"]).unwrap();

    assert!(matches!(parsed.command, CliCommand::Gui));
}

#[test]
fn build_command_defaults_to_both_and_current_platform() {
    let parsed = Cli::try_parse_from(["mcmpu", "build"]).unwrap();

    assert!(matches!(
        parsed.command,
        CliCommand::Build {
            target: CliBuildTarget::Both,
            server_platform: None,
            output: None,
        }
    ));
}

#[test]
fn build_command_parses_target_platform_and_output() {
    let parsed = Cli::try_parse_from([
        "mcmpu",
        "build",
        "--target",
        "server",
        "--server-platform",
        "linux-x86_64",
        "--output",
        "F:/external/output",
    ])
    .unwrap();

    assert!(matches!(
        parsed.command,
        CliCommand::Build {
            target: CliBuildTarget::Server,
            server_platform: Some(CliServerPlatform::LinuxX86_64),
            output: Some(ref path),
        } if path == std::path::Path::new("F:/external/output")
    ));
}

#[test]
fn build_rejects_server_platform_for_client_target_before_loading_project() {
    let mut command = Command::cargo_bin("mcmpu").unwrap();

    command
        .args([
            "build",
            "--target",
            "client",
            "--server-platform",
            "windows-x86_64",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "--server-platform cannot be used with --target client",
        ));
}

#[test]
fn check_fails_when_api_key_is_empty() {
    if mcmpu::curseforge::resolver::ApiKeyResolver::builtin_key().is_some() {
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    let config = support::sample_config();
    std::fs::write(
        temp.path().join("config.toml"),
        toml::to_string_pretty(&config).unwrap(),
    )
    .unwrap();

    let mut command = Command::cargo_bin("mcmpu").unwrap();

    command
        .current_dir(temp.path())
        .env("MCMPU_DISABLE_PERSISTENT_ENV", "1")
        .env_remove("MCMODPACKUTIL_CURSEFORGE_API_KEY")
        .env_remove("CURSEFORGE_API_KEY")
        .env_remove("CF_API_KEY")
        .arg("check")
        .assert()
        .failure()
        .stderr(predicate::str::contains("CurseForge API key"));
}

#[test]
fn check_rejects_curseforge_api_key_saved_in_config() {
    let temp = tempfile::tempdir().unwrap();
    let config = format!(
        "{}\n[curseforge]\ncurseforge_api_key = \"legacy\"\n",
        toml::to_string_pretty(&support::sample_config()).unwrap()
    );
    std::fs::write(temp.path().join("config.toml"), config).unwrap();
    support::write_empty_lists(temp.path());

    let mut command = Command::cargo_bin("mcmpu").unwrap();

    command
        .current_dir(temp.path())
        .env("MCMPU_DISABLE_PERSISTENT_ENV", "1")
        .env_remove("MCMODPACKUTIL_CURSEFORGE_API_KEY")
        .env_remove("CURSEFORGE_API_KEY")
        .env_remove("CF_API_KEY")
        .arg("check")
        .assert()
        .failure()
        .stderr(predicate::str::contains("[curseforge]"))
        .stderr(predicate::str::contains("--curseforge-api-key"))
        .stderr(predicate::str::contains("MCMODPACKUTIL_CURSEFORGE_API_KEY"))
        .stderr(predicate::str::contains("official build"));
}

#[test]
fn check_accepts_api_key_from_environment() {
    let temp = tempfile::tempdir().unwrap();
    let mut config = support::sample_config();
    config.language = "en-US".to_string();
    config.pack.loader = mcmpu::config::LoaderKind::Forge;
    config.pack.forge_compat_version = None;
    config.cleanroom = None;
    std::fs::write(
        temp.path().join("config.toml"),
        toml::to_string_pretty(&config).unwrap(),
    )
    .unwrap();
    support::write_empty_lists(temp.path());

    let mut command = Command::cargo_bin("mcmpu").unwrap();

    command
        .current_dir(temp.path())
        .env("MCMODPACKUTIL_CURSEFORGE_API_KEY", "secret")
        .arg("check")
        .assert()
        .success()
        .stdout(predicate::str::contains("check passed"));
}

#[test]
fn check_accepts_api_key_from_common_environment_alias() {
    let temp = tempfile::tempdir().unwrap();
    let mut config = support::sample_config();
    config.language = "en-US".to_string();
    config.pack.loader = mcmpu::config::LoaderKind::Forge;
    config.pack.forge_compat_version = None;
    config.cleanroom = None;
    std::fs::write(
        temp.path().join("config.toml"),
        toml::to_string_pretty(&config).unwrap(),
    )
    .unwrap();
    support::write_empty_lists(temp.path());

    let mut command = Command::cargo_bin("mcmpu").unwrap();

    command
        .current_dir(temp.path())
        .env("MCMPU_DISABLE_PERSISTENT_ENV", "1")
        .env_remove("MCMODPACKUTIL_CURSEFORGE_API_KEY")
        .env("CURSEFORGE_API_KEY", "secret")
        .arg("check")
        .assert()
        .success()
        .stdout(predicate::str::contains("check passed"));
}

#[test]
fn check_rejects_invalid_explicit_proxy_before_network_access() {
    let temp = tempfile::tempdir().unwrap();
    let mut config = support::sample_config();
    config.language = "en-US".to_string();
    config.pack.loader = mcmpu::config::LoaderKind::Forge;
    config.pack.forge_compat_version = None;
    config.cleanroom = None;
    std::fs::write(
        temp.path().join("config.toml"),
        toml::to_string_pretty(&config).unwrap(),
    )
    .unwrap();
    support::write_empty_lists(temp.path());

    let mut command = Command::cargo_bin("mcmpu").unwrap();

    command
        .current_dir(temp.path())
        .env("MCMODPACKUTIL_CURSEFORGE_API_KEY", "secret")
        .args(["--proxy", "not a proxy url", "check"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("invalid proxy URL"))
        .stderr(predicate::str::contains("secret").not());
}

#[test]
fn check_rejects_invalid_configured_proxy_before_network_access() {
    let temp = tempfile::tempdir().unwrap();
    let mut config = support::sample_config();
    config.language = "en-US".to_string();
    config.pack.loader = mcmpu::config::LoaderKind::Forge;
    config.pack.forge_compat_version = None;
    config.cleanroom = None;
    config.network.proxy_ip = "not a proxy host".to_string();
    config.network.proxy_port = 7890;
    std::fs::write(
        temp.path().join("config.toml"),
        toml::to_string_pretty(&config).unwrap(),
    )
    .unwrap();
    support::write_empty_lists(temp.path());

    let mut command = Command::cargo_bin("mcmpu").unwrap();

    command
        .current_dir(temp.path())
        .env("MCMODPACKUTIL_CURSEFORGE_API_KEY", "secret")
        .arg("check")
        .assert()
        .failure()
        .stderr(predicate::str::contains("invalid proxy URL"))
        .stderr(predicate::str::contains("secret").not());
}

#[test]
fn init_command_writes_project_files() {
    let temp = tempfile::tempdir().unwrap();
    let zip_path = temp.path().join("pack.zip");
    let out = temp.path().join("project");
    support::write_empty_manifest_zip(&zip_path);

    let mut command = Command::cargo_bin("mcmpu").unwrap();

    command
        .current_dir(temp.path())
        .args([
            "--lang",
            "en-US",
            "init",
            zip_path.to_str().unwrap(),
            "--output",
            out.to_str().unwrap(),
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("init completed"));

    assert!(out.join("config.toml").is_file());
    assert!(out.join("common.json").is_file());
    assert!(out.join("overrides/common/config/example.cfg").is_file());
}

#[test]
fn init_detects_language_from_environment() {
    let temp = tempfile::tempdir().unwrap();
    let zip_path = temp.path().join("pack.zip");
    let out = temp.path().join("project");
    support::write_empty_manifest_zip(&zip_path);

    let mut command = Command::cargo_bin("mcmpu").unwrap();

    command
        .current_dir(temp.path())
        .env("LANGUAGE", "zh-CN")
        .env_remove("LC_ALL")
        .env_remove("LANG")
        .args([
            "init",
            zip_path.to_str().unwrap(),
            "--output",
            out.to_str().unwrap(),
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("初始化完成"));
}

/// The run commands launch and supervise the game, so their help must not describe them as pure
/// preparation steps, and the options must document what they override.
#[test]
fn help_describes_the_run_commands_and_option_precedence() {
    let mut command = Command::cargo_bin("mcmpu").unwrap();
    command
        .arg("--help")
        .assert()
        .success()
        .stdout(predicate::str::contains("supervise"))
        .stdout(predicate::str::contains("config.toml"))
        // The API key option names the environment variables it falls back to.
        .stdout(predicate::str::contains("CURSEFORGE_API_KEY"))
        .stdout(predicate::str::contains("http://127.0.0.1:7890"));

    // A stale description would claim the run commands only prepare a directory.
    let mut command = Command::cargo_bin("mcmpu").unwrap();
    command
        .arg("--help")
        .assert()
        .success()
        .stdout(predicate::str::contains("Prepare the client run directory").not());
}
