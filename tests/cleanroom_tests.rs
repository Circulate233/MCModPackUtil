use std::io::{Read, Write};
use std::net::TcpListener;

use mcmpu::config::{CleanroomConfig, Config};
use mcmpu::dependency::{DependencySide, DependencyType};
use mcmpu::project::cleanroom::{
    cleanroom_dependency_supplements, cleanroom_dependency_supplements_for,
    validate_cleanroom_config,
};
use mcmpu::project::cleanroom_relauncher::{
    HttpMmcPackSource, MmcPackSource, prepare_client_config,
};
use sha1::{Digest, Sha1};

mod support;

#[test]
fn cleanroom_supplements_include_relauncher_client_and_fugue_common() {
    let config = cleanroom_config(false, 3);

    let entries = cleanroom_dependency_supplements(&config).unwrap();

    let relauncher = entries
        .iter()
        .find(|entry| entry.project_id == Some(1_214_906))
        .expect("relauncher entry");
    assert_eq!(
        relauncher.slug.as_deref(),
        Some("cleanroom-relauncher-unofficial")
    );
    assert_eq!(relauncher.file_id, None);
    assert_eq!(relauncher.side, DependencySide::Client);
    assert_eq!(relauncher.dependency_type, DependencyType::Mod);

    let fugue = entries
        .iter()
        .find(|entry| entry.project_id == Some(1_005_815))
        .expect("fugue entry");
    assert_eq!(fugue.slug.as_deref(), Some("fugue"));
    assert_eq!(fugue.file_id, None);
    assert_eq!(fugue.side, DependencySide::Common);
}

#[test]
fn cleanroom_supplements_skip_scalar_when_disabled() {
    let config = cleanroom_config(false, 3);

    let entries = cleanroom_dependency_supplements(&config).unwrap();

    assert!(entries.iter().all(|entry| {
        entry.project_id != Some(1_235_372) && entry.project_id != Some(1_012_168)
    }));
}

#[test]
fn cleanroom_supplements_skip_fugue_when_user_already_lists_it() {
    let config = cleanroom_config(false, 3);
    let existing = vec![entry(1_005_815, "fugue")];

    let entries = cleanroom_dependency_supplements_for(&config, &existing).unwrap();

    assert!(
        entries
            .iter()
            .all(|entry| entry.project_id != Some(1_005_815))
    );
}

#[test]
fn cleanroom_supplements_skip_scalar_when_user_already_lists_scalar() {
    let config = cleanroom_config(true, 3);
    let existing = vec![entry(1_235_372, "scalar")];

    let entries = cleanroom_dependency_supplements_for(&config, &existing).unwrap();

    assert!(entries.iter().all(|entry| {
        entry.project_id != Some(1_235_372) && entry.project_id != Some(1_012_168)
    }));
}

#[test]
fn cleanroom_supplements_skip_relauncher_when_any_relauncher_variant_exists() {
    for project_id in [1_214_490, 1_336_201, 1_214_906] {
        let config = cleanroom_config(false, 3);
        let existing = vec![entry(project_id, "existing-relauncher")];

        let entries = cleanroom_dependency_supplements_for(&config, &existing).unwrap();

        assert!(entries.iter().all(|entry| {
            !matches!(
                entry.project_id,
                Some(1_214_490) | Some(1_336_201) | Some(1_214_906)
            )
        }));
    }
}

#[test]
fn cleanroom_relauncher_config_selects_original_variant() {
    let mut config = cleanroom_config(false, 3);
    config.cleanroom.as_mut().unwrap().relauncher = mcmpu::config::CleanroomRelauncher::Original;

    let entries = cleanroom_dependency_supplements(&config).unwrap();

    let relauncher = entries
        .iter()
        .find(|entry| entry.project_id == Some(1_214_490))
        .expect("original relauncher entry");
    assert_eq!(relauncher.slug.as_deref(), Some("cleanroom-relauncher"));
    assert_eq!(relauncher.side, DependencySide::Client);
}

#[test]
fn cleanroom_relauncher_config_selects_improved_variant() {
    let mut config = cleanroom_config(false, 3);
    config.cleanroom.as_mut().unwrap().relauncher = mcmpu::config::CleanroomRelauncher::Improved;

    let entries = cleanroom_dependency_supplements(&config).unwrap();

    let relauncher = entries
        .iter()
        .find(|entry| entry.project_id == Some(1_336_201))
        .expect("improved relauncher entry");
    assert_eq!(
        relauncher.slug.as_deref(),
        Some("improved-cleanroom-relauncher")
    );
    assert_eq!(relauncher.side, DependencySide::Client);
}

#[test]
fn cleanroom_scalar_two_uses_exact_project_and_file() {
    let config = cleanroom_config(true, 2);

    let entries = cleanroom_dependency_supplements(&config).unwrap();

    let scalar = entries
        .iter()
        .find(|entry| entry.project_id == Some(1_235_372))
        .expect("scalar 2 entry");
    assert_eq!(scalar.file_id, Some(6_842_491));
    assert_eq!(scalar.side, DependencySide::Common);
}

#[test]
fn cleanroom_scalar_three_uses_latest_project() {
    let config = cleanroom_config(true, 3);

    let entries = cleanroom_dependency_supplements(&config).unwrap();

    let scalar = entries
        .iter()
        .find(|entry| entry.project_id == Some(1_012_168))
        .expect("scalar 3 entry");
    assert_eq!(scalar.file_id, None);
    assert_eq!(scalar.side, DependencySide::Common);
}

#[test]
fn cleanroom_scalar_rejects_unsupported_version() {
    let config = cleanroom_config(true, 4);

    let err = cleanroom_dependency_supplements(&config).unwrap_err();

    assert!(err.to_string().contains("scalar_version"));
    assert!(err.to_string().contains("2 or 3"));
}

#[test]
fn cleanroom_section_is_invalid_for_non_cleanroom_loader() {
    let mut config = support::sample_config();
    config.pack.loader = mcmpu::config::LoaderKind::Forge;
    config.cleanroom = Some(CleanroomConfig {
        use_scalar: true,
        scalar_version: 3,
        relauncher: mcmpu::config::CleanroomRelauncher::Unofficial,
    });

    let err = validate_cleanroom_config(&config).unwrap_err();

    assert!(err.to_string().contains("cleanroom"));
}

#[test]
fn cleanroom_relauncher_configs_use_variant_schema_and_client_memory() {
    let project = tempfile::tempdir().unwrap();
    let mut config = cleanroom_config(false, 3);
    config.pack.loader_version = "0.6.2-alpha".to_string();
    config.java.client.min_memory = "3G".to_string();
    config.java.client.max_memory = "6G".to_string();
    config.java.client.extra_args = vec![
        "-XX:+UseZGC  -XX:+UseCompactObjectHeaders\t-XX:ReservedCodeCacheSize=500M".to_string(),
        "-XX:-DontCompileHugeMethods".to_string(),
    ];
    let expected_args = "-Xms3G -Xmx6G -XX:+UseZGC -XX:+UseCompactObjectHeaders -XX:ReservedCodeCacheSize=500M -XX:-DontCompileHugeMethods";

    let original = entry(1_214_490, "cleanroom-relauncher");
    let generated = prepare_client_config(project.path(), &config, &[original])
        .unwrap()
        .unwrap();
    assert_eq!(generated.archive_path, "overrides/config/relauncher.json");
    let json: serde_json::Value = serde_json::from_slice(&generated.bytes).unwrap();
    assert_eq!(json["selectedVersion"], "0.6.2-alpha");
    assert_eq!(json["args"], expected_args);
    assert_eq!(json["FetchUpdates"], false);

    let improved = entry(1_336_201, "improved-cleanroom-relauncher");
    let generated = prepare_client_config(project.path(), &config, &[improved])
        .unwrap()
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&generated.bytes).unwrap();
    assert_eq!(json["selectedVersion"], "0.6.2-alpha");
    assert_eq!(json["args"], expected_args);
    assert_eq!(json["autoUpdate"], false);
    assert_eq!(json["javaVendor"], "adoptium");

    let unofficial = entry(1_214_906, "cleanroom-relauncher-unofficial");
    let generated = prepare_client_config(project.path(), &config, &[unofficial])
        .unwrap()
        .unwrap();
    assert_eq!(
        generated.archive_path,
        "overrides/config/cleanroom_relauncher.cfg"
    );
    let cfg = String::from_utf8(generated.bytes).unwrap();
    let fields = parse_unofficial_cfg_fields(&cfg).unwrap();
    assert_eq!(
        fields.get("Show Config"),
        Some(&UnofficialCfgValue::Boolean(true))
    );
    assert_eq!(
        fields.get("Always Show Config"),
        Some(&UnofficialCfgValue::Boolean(false))
    );
    assert_eq!(
        fields.get("Java Path"),
        Some(&UnofficialCfgValue::String(String::new()))
    );
    assert_eq!(
        fields.get("JVM Arguments"),
        Some(&UnofficialCfgValue::String(expected_args.to_string()))
    );
    assert_eq!(
        fields.get("Use Local MMC Pack"),
        Some(&UnofficialCfgValue::Boolean(true))
    );
    assert_eq!(
        fields.get("Local MMC Pack Path"),
        Some(&UnofficialCfgValue::String(
            "cleanroom/cleanroom-0.6.2-alpha.zip".to_string()
        ))
    );
}

#[test]
fn cleanroom_relauncher_config_rejects_ambiguous_or_invalid_inputs() {
    let project = tempfile::tempdir().unwrap();
    let config = cleanroom_config(false, 3);
    let err = prepare_client_config(
        project.path(),
        &config,
        &[
            entry(1_214_490, "cleanroom-relauncher"),
            entry(1_336_201, "improved-cleanroom-relauncher"),
        ],
    )
    .unwrap_err();
    assert!(err.to_string().contains("exactly one"));

    let mut config = cleanroom_config(false, 3);
    config.java.client.extra_args = vec!["-Xmx8G".to_string()];
    let err = prepare_client_config(
        project.path(),
        &config,
        &[entry(1_214_906, "cleanroom-relauncher-unofficial")],
    )
    .unwrap_err();
    assert!(err.to_string().contains("-Xmx"));

    let mut config = cleanroom_config(false, 3);
    config.java.client.min_memory = "3 G".to_string();
    let err = prepare_client_config(
        project.path(),
        &config,
        &[entry(1_214_906, "cleanroom-relauncher-unofficial")],
    )
    .unwrap_err();
    assert!(err.to_string().contains("whitespace-free"));
}

#[test]
fn cleanroom_http_source_bounds_sidecars_and_streams_verified_archives() {
    let archive = support::fixture_mmc_pack("1.0.0");
    let expected_sha1 = format!("{:x}", Sha1::digest(&archive));
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let served_archive = archive.clone();
    let server = std::thread::spawn(move || {
        let mut requests = Vec::new();
        for _ in 0..2 {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0_u8; 4096];
            let read = stream.read(&mut request).unwrap();
            let request = String::from_utf8_lossy(&request[..read]).into_owned();
            let path = request
                .lines()
                .next()
                .and_then(|line| line.split_whitespace().nth(1))
                .unwrap();
            let body = if path == "/oversized.sha1" {
                vec![b'0'; 4097]
            } else {
                served_archive.clone()
            };
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            )
            .unwrap();
            stream.write_all(&body).unwrap();
            requests.push(request);
        }
        requests
    });
    let source = HttpMmcPackSource::with_proxy(None).unwrap();

    let oversized = source
        .fetch(&format!("http://{address}/oversized.sha1"))
        .unwrap_err();
    assert!(oversized.to_string().contains("4096-byte limit"));

    let temp = tempfile::tempdir().unwrap();
    let target = temp.path().join("cache/cleanroom.zip");
    source
        .download_archive(
            &format!("http://{address}/cleanroom.zip"),
            &target,
            &expected_sha1,
        )
        .unwrap();

    assert_eq!(std::fs::read(&target).unwrap(), archive);
    let requests = server.join().unwrap();
    assert!(
        requests[1]
            .to_ascii_lowercase()
            .contains("range: bytes=0-0")
    );
    assert!(!target.parent().unwrap().read_dir().unwrap().any(|entry| {
        entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .contains("part")
    }));
}

#[derive(Debug, PartialEq, Eq)]
enum UnofficialCfgValue {
    Boolean(bool),
    Integer(i32),
    String(String),
}

fn parse_unofficial_cfg_fields(
    content: &str,
) -> Result<std::collections::BTreeMap<String, UnofficialCfgValue>, String> {
    content
        .lines()
        .filter_map(|line| {
            let line = line.trim_start();
            let kind = line.chars().next()?;
            let line = line.strip_prefix(&format!("{kind}:\""))?;
            let (key, value) = line.split_once("\"=")?;
            Some((kind, key.to_string(), value.to_string()))
        })
        .map(|(kind, key, value)| {
            let value = match kind {
                'B' => UnofficialCfgValue::Boolean(value.parse().map_err(|_| {
                    format!("invalid boolean value '{value}' for Cleanroom field '{key}'")
                })?),
                'I' => UnofficialCfgValue::Integer(value.parse().map_err(|_| {
                    format!("invalid integer value '{value}' for Cleanroom field '{key}'")
                })?),
                'S' => UnofficialCfgValue::String(value),
                _ => return Err(format!("unsupported Cleanroom CFG field type '{kind}'")),
            };
            Ok((key, value))
        })
        .collect()
}

fn cleanroom_config(use_scalar: bool, scalar_version: u8) -> Config {
    let mut config = support::sample_config();
    config.cleanroom = Some(CleanroomConfig {
        use_scalar,
        scalar_version,
        relauncher: mcmpu::config::CleanroomRelauncher::Unofficial,
    });
    config
}

fn entry(project_id: u64, slug: &str) -> mcmpu::dependency::DependencyEntry {
    mcmpu::dependency::DependencyEntry {
        project_id: Some(project_id),
        file_id: Some(1),
        slug: Some(slug.to_string()),
        loader: Some("forge".to_string()),
        side: DependencySide::Common,
        dependency_type: DependencyType::Mod,
        name: slug.to_string(),
        authors: Vec::new(),
        required: true,
    }
}
