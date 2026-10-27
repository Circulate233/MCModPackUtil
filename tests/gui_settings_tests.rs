mod support;

use mcmpu::config::{Config, LoaderKind, TestRunConfig};
use mcmpu::gui::settings::{SettingsError, SettingsForm};
use tempfile::tempdir;

#[test]
fn settings_form_roundtrips_config_values() {
    let mut config = support::sample_config();
    config.language = "en-US".to_string();
    config.pack.name = "Edited Pack".to_string();
    config.pack.loader = LoaderKind::Forge;
    config.pack.forge_compat_version = None;
    config.network.proxy_ip = "127.0.0.1".to_string();
    config.network.proxy_port = 7890;
    config.java.client.extra_args = vec!["-Dclient=true".to_string()];
    config.cleanroom = None;

    let form = SettingsForm::from_config(config.clone());
    let roundtrip = form.to_config().unwrap();

    assert_eq!(roundtrip, config);
}

#[test]
fn settings_form_saves_pretty_toml_that_config_loader_reads() {
    let dir = tempdir().unwrap();
    std::fs::write(
        dir.path().join("config.toml"),
        toml::to_string_pretty(&support::sample_config()).unwrap(),
    )
    .unwrap();

    let mut form = SettingsForm::load(dir.path()).unwrap();
    form.pack_name = "Saved Pack".to_string();
    form.authors = "Alice, Bob".to_string();
    form.proxy_ip = "10.0.0.2".to_string();
    form.proxy_port = "8080".to_string();
    form.loader = LoaderKind::Fabric;
    form.loader_version = "0.16.14".to_string();
    form.save(dir.path()).unwrap();

    let saved = Config::from_path(dir.path().join("config.toml")).unwrap();
    assert_eq!(saved.pack.name, "Saved Pack");
    assert_eq!(saved.pack.authors, vec!["Alice", "Bob"]);
    assert_eq!(saved.network.proxy_ip, "10.0.0.2");
    assert_eq!(saved.network.proxy_port, 8080);
    assert_eq!(saved.cleanroom, None);
    let main_config = std::fs::read_to_string(dir.path().join("config.toml")).unwrap();
    assert!(!main_config.contains("testRun"));
    assert!(!dir.path().join("test-run.toml").exists());
}

#[test]
fn settings_form_loads_a_partial_config_and_can_complete_missing_java_settings() {
    let dir = tempdir().unwrap();
    std::fs::write(
        dir.path().join("config.toml"),
        r#"
language = "zh-CN"

[pack]
name = "Partial Pack"
version = "1.0.0"
authors = ["Alice"]
minecraft_version = "1.12.2"
loader = "forge"
loader_version = "14.23.5.2860"

[build]
output_dir = "dist"

[java.client]
min_memory = "2G"
max_memory = "4G"
"#,
    )
    .unwrap();

    let mut form = SettingsForm::load(dir.path()).unwrap();
    assert_eq!(form.java_version_major, 8);
    assert_eq!(form.server_min_memory, "2G");
    assert_eq!(form.server_max_memory, "4G");

    form.java_version_major = 17;
    form.save(dir.path()).unwrap();

    let completed = Config::from_path(dir.path().join("config.toml")).unwrap();
    assert_eq!(completed.java.version_major, 17);
    assert_eq!(completed.java.server.min_memory, "2G");
    assert_eq!(completed.java.server.max_memory, "4G");
}

#[test]
fn settings_form_opens_when_config_file_has_not_been_created() {
    let dir = tempdir().unwrap();

    let form = SettingsForm::load(dir.path()).unwrap();

    assert!(form.language.is_empty());
    assert_eq!(form.java_version_major, 8);
    assert_eq!(form.output_dir, "dist");
}

#[test]
fn settings_form_reads_a_valid_legacy_java_value_without_a_main_config() {
    let dir = tempdir().unwrap();
    std::fs::write(
        dir.path().join("test-run.toml"),
        "java_version_major = 21\n",
    )
    .unwrap();

    let form = SettingsForm::load(dir.path()).unwrap();

    assert_eq!(form.java_version_major, 21);
    assert!(!dir.path().join("config.toml").exists());
}

#[test]
fn settings_form_propagates_legacy_java_non_content_io_errors() {
    let dir = tempdir().unwrap();
    std::fs::create_dir(dir.path().join("test-run.toml")).unwrap();

    let error = SettingsForm::load(dir.path()).unwrap_err();

    assert!(matches!(
        error,
        SettingsError::Config(mcmpu::config::ConfigError::Read { .. })
    ));
}

#[test]
fn settings_form_uses_defaults_when_config_toml_is_malformed() {
    let dir = tempdir().unwrap();
    std::fs::write(
        dir.path().join("config.toml"),
        "language = [this is not valid TOML",
    )
    .unwrap();

    let form = SettingsForm::load(dir.path()).unwrap();

    assert!(form.language.is_empty());
    assert!(form.pack_name.is_empty());
    assert!(form.loader_version.is_empty());
    assert_eq!(form.output_dir, "dist");
    assert_eq!(form.java_version_major, 8);
    assert!(Config::from_path(dir.path().join("config.toml")).is_err());
}

#[test]
fn settings_form_ignores_unknown_and_wrong_typed_fields_independently() {
    let dir = tempdir().unwrap();
    std::fs::write(
        dir.path().join("config.toml"),
        r#"
language = 42
future_option = "kept out of the editor"

[pack]
name = "Readable Pack"
version = false
authors = ["Alice", 17]
minecraft_version = "1.20.1"
loader = "future-loader"
loader_version = 123
future_pack_option = true

[network]
proxy_ip = "127.0.0.1"
proxy_port = "7890"

[build]
output_dir = 12

[java]
version_major = "21"

[java.client]
min_memory = "3G"
max_memory = false
extra_args = ["-Dvalid=true", 1]

[java.server]
min_memory = 2
max_memory = "6G"

[cleanroom]
use_scalar = "yes"
scalar_version = 9
relauncher = "future"
unknown = "ignored"
"#,
    )
    .unwrap();

    let form = SettingsForm::load(dir.path()).unwrap();

    assert!(form.language.is_empty());
    assert_eq!(form.pack_name, "Readable Pack");
    assert!(form.pack_version.is_empty());
    assert!(form.authors.is_empty());
    assert_eq!(form.minecraft_version, "1.20.1");
    assert_eq!(form.loader, LoaderKind::Forge);
    assert!(form.loader_version.is_empty());
    assert!(form.proxy_ip.is_empty());
    assert!(form.proxy_port.is_empty());
    assert_eq!(form.output_dir, "dist");
    assert_eq!(form.java_version_major, 8);
    assert_eq!(form.client_min_memory, "3G");
    assert_eq!(form.client_max_memory, "4G");
    assert!(form.client_extra_args.is_empty());
    assert_eq!(form.server_min_memory, "2G");
    assert_eq!(form.server_max_memory, "6G");
    assert!(form.cleanroom_enabled);
    assert!(!form.cleanroom_use_scalar);
    assert_eq!(form.cleanroom_scalar_version, "2");
}

#[test]
fn settings_form_defaults_invalid_semantic_values_but_save_remains_strict() {
    let dir = tempdir().unwrap();
    std::fs::write(
        dir.path().join("config.toml"),
        r#"
language = "unsupported"

[pack]
name = ""
version = ""
authors = []
minecraft_version = ""
loader = "forge"
loader_version = ""

[network]
proxy_ip = "127.0.0.1"
proxy_port = 0

[build]
output_dir = "../outside"

[java]
version_major = 0

[java.client]
min_memory = "2G"
max_memory = "4G"

[java.server]
min_memory = "2G"
max_memory = "4G"
"#,
    )
    .unwrap();

    let form = SettingsForm::load(dir.path()).unwrap();

    assert!(form.language.is_empty());
    assert_eq!(form.output_dir, "dist");
    assert_eq!(form.java_version_major, 8);
    assert!(form.proxy_ip.is_empty());
    assert!(form.proxy_port.is_empty());
    assert!(matches!(
        form.to_config().unwrap_err(),
        SettingsError::MissingLoaderVersion
    ));
}

#[test]
fn settings_form_ignores_legacy_key_sections_that_strict_loading_rejects() {
    let dir = tempdir().unwrap();
    let config = toml::to_string_pretty(&support::sample_config()).unwrap();
    std::fs::write(
        dir.path().join("config.toml"),
        format!("{config}\n[curseforge]\napi_key = \"legacy-key\"\n"),
    )
    .unwrap();

    let form = SettingsForm::load(dir.path()).unwrap();

    assert_eq!(form.pack_name, support::sample_config().pack.name);
    assert!(Config::from_path(dir.path().join("config.toml")).is_err());
}

#[test]
fn settings_form_uses_only_valid_legacy_java_values() {
    let dir = tempdir().unwrap();
    let mut value = toml::Value::try_from(support::sample_config()).unwrap();
    value
        .get_mut("java")
        .and_then(toml::Value::as_table_mut)
        .unwrap()
        .remove("version_major");
    std::fs::write(
        dir.path().join("config.toml"),
        toml::to_string_pretty(&value).unwrap(),
    )
    .unwrap();
    std::fs::write(dir.path().join("test-run.toml"), "java_version_major = 0\n").unwrap();

    let form = SettingsForm::load(dir.path()).unwrap();

    assert_eq!(form.java_version_major, 8);
}

#[test]
fn settings_form_displays_an_explicit_legacy_local_java_version_without_writing_files() {
    let dir = tempdir().unwrap();
    let mut config = support::sample_config();
    config.java.version_major = 8;
    let mut config_value = toml::Value::try_from(config).unwrap();
    config_value
        .get_mut("java")
        .and_then(toml::Value::as_table_mut)
        .unwrap()
        .remove("version_major");
    std::fs::write(
        dir.path().join("config.toml"),
        toml::to_string_pretty(&config_value).unwrap(),
    )
    .unwrap();
    std::fs::write(
        dir.path().join("test-run.toml"),
        r#"
player_name = "Player"
download_source = "auto"
java_version_major = 25
sync_interval_seconds = 30
debug_extra_jvm_args = []
"#,
    )
    .unwrap();
    let before_config = std::fs::read(dir.path().join("config.toml")).unwrap();
    let before_test_run = std::fs::read(dir.path().join("test-run.toml")).unwrap();

    let form = SettingsForm::load(dir.path()).unwrap();

    assert_eq!(form.java_version_major, 25);
    assert_eq!(
        std::fs::read(dir.path().join("config.toml")).unwrap(),
        before_config
    );
    assert_eq!(
        std::fs::read(dir.path().join("test-run.toml")).unwrap(),
        before_test_run
    );
}

#[test]
fn settings_save_does_not_modify_existing_test_run_toml() {
    let dir = tempdir().unwrap();
    std::fs::write(
        dir.path().join("config.toml"),
        toml::to_string_pretty(&support::sample_config()).unwrap(),
    )
    .unwrap();
    let local_run = TestRunConfig {
        player_name: "LocalPlayer".to_string(),
        ..TestRunConfig::default()
    };
    local_run.write_to_project(dir.path()).unwrap();
    let original = std::fs::read(dir.path().join("test-run.toml")).unwrap();
    let local_toml =
        toml::from_str::<toml::Value>(std::str::from_utf8(&original).unwrap()).unwrap();
    assert!(local_toml.get("server_version_major").is_none());

    let mut form = SettingsForm::load(dir.path()).unwrap();
    form.pack_name = "Config Only".to_string();
    form.save(dir.path()).unwrap();

    assert_eq!(
        std::fs::read(dir.path().join("test-run.toml")).unwrap(),
        original
    );
}

#[test]
fn non_cleanroom_settings_do_not_write_forge_compatibility_or_cleanroom_config() {
    let mut form = SettingsForm::from_config(support::sample_config());
    form.loader = LoaderKind::Fabric;
    form.loader_version = "0.16.14".to_string();
    form.forge_compat_version = "14.23.5.2860".to_string();

    let config = form.to_config().unwrap();

    assert_eq!(config.pack.forge_compat_version, None);
    assert_eq!(config.cleanroom, None);
}

#[test]
fn cleanroom_settings_require_selected_forge_compatibility_version() {
    let mut form = SettingsForm::from_config(support::sample_config());
    form.loader = LoaderKind::Cleanroom;
    form.forge_compat_version.clear();

    assert!(matches!(
        form.to_config().unwrap_err(),
        SettingsError::MissingForgeCompatVersion
    ));
}

#[test]
fn settings_form_rejects_invalid_proxy_port() {
    let mut form = SettingsForm::from_config(support::sample_config());
    form.proxy_ip = "127.0.0.1".to_string();
    form.proxy_port = "70000".to_string();

    assert!(matches!(
        form.to_config().unwrap_err(),
        SettingsError::InvalidProxyPort
    ));
}

#[test]
fn settings_save_rejects_missing_minecraft_version_without_modifying_config() {
    let dir = tempdir().unwrap();
    let config_path = dir.path().join("config.toml");
    let original = toml::to_string_pretty(&support::sample_config()).unwrap();
    std::fs::write(&config_path, &original).unwrap();
    let mut form = SettingsForm::load(dir.path()).unwrap();
    form.minecraft_version.clear();

    let error = form.save(dir.path()).unwrap_err();

    assert!(matches!(
        error,
        SettingsError::Config(mcmpu::config::ConfigError::MissingMinecraftVersion)
    ));
    assert_eq!(std::fs::read_to_string(config_path).unwrap(), original);
}

#[test]
fn settings_form_rejects_invalid_cleanroom_scalar_version() {
    let mut form = SettingsForm::from_config(support::sample_config());
    form.cleanroom_scalar_version = "4".to_string();

    assert!(matches!(
        form.to_config().unwrap_err(),
        SettingsError::InvalidScalarVersion
    ));
}
