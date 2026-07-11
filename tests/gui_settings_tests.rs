mod support;

use mcmpu::config::{CleanroomRelauncher, Config, DownloadSource, LoaderKind};
use mcmpu::gui::settings::{SettingsError, SettingsForm};
use tempfile::tempdir;

#[test]
fn settings_form_roundtrips_config_values() {
    let mut config = support::sample_config();
    config.language = "en-US".to_string();
    config.pack.name = "Edited Pack".to_string();
    config.pack.loader = LoaderKind::Forge;
    config.network.proxy_ip = "127.0.0.1".to_string();
    config.network.proxy_port = 7890;
    config.java.client.extra_args = vec!["-Dclient=true".to_string()];
    config.test_run.player_name = "DevPlayer".to_string();
    config.test_run.download_source = DownloadSource::Bmclapi;
    config.test_run.java_version_major = 17;
    config.test_run.sync_interval_seconds = 45;
    config.test_run.debug_extra_jvm_args = vec!["-Ddebug=true".to_string()];
    config.test_run.java_path = "F:/java/bin/java.exe".to_string();
    config.cleanroom.as_mut().unwrap().use_scalar = true;
    config.cleanroom.as_mut().unwrap().scalar_version = 3;
    config.cleanroom.as_mut().unwrap().relauncher = CleanroomRelauncher::Improved;

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
    form.cleanroom_enabled = false;
    form.save(dir.path()).unwrap();

    let saved = Config::from_path(dir.path().join("config.toml")).unwrap();
    assert_eq!(saved.pack.name, "Saved Pack");
    assert_eq!(saved.pack.authors, vec!["Alice", "Bob"]);
    assert_eq!(saved.network.proxy_ip, "10.0.0.2");
    assert_eq!(saved.network.proxy_port, 8080);
    assert_eq!(saved.cleanroom, None);
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
fn settings_form_rejects_invalid_java_version() {
    let mut form = SettingsForm::from_config(support::sample_config());
    form.test_java_version_major = "0".to_string();

    assert!(matches!(
        form.to_config().unwrap_err(),
        SettingsError::InvalidJavaVersion
    ));
}

#[test]
fn settings_form_rejects_invalid_sync_interval() {
    let mut form = SettingsForm::from_config(support::sample_config());
    form.test_sync_interval_seconds = "0".to_string();

    assert!(matches!(
        form.to_config().unwrap_err(),
        SettingsError::InvalidSyncInterval
    ));
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
