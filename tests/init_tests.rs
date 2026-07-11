use std::collections::HashMap;
use std::sync::Mutex;

use mcmpu::curseforge::model::{CfFile, CfProject};
use mcmpu::curseforge::resolver::{CurseForgeApi, ResolveError};
use mcmpu::project::init::{
    InitProgress, initialize_from_zip, initialize_from_zip_with_api,
    initialize_from_zip_with_api_and_progress,
};

mod support;

#[test]
fn init_rejects_zip_without_manifest_json() {
    let temp = tempfile::tempdir().unwrap();
    let zip_path = temp.path().join("pack.zip");
    support::write_zip(
        &zip_path,
        &[("overrides/config/test.cfg", b"value".as_slice())],
    );

    let err = initialize_from_zip(&zip_path, temp.path().join("out"), "zh-CN").unwrap_err();

    assert!(err.to_string().contains("manifest.json"));
}

#[test]
fn init_rejects_illegal_manifest_version() {
    let temp = tempfile::tempdir().unwrap();
    let zip_path = temp.path().join("pack.zip");
    support::write_zip(
        &zip_path,
        &[(
            "manifest.json",
            manifest_with_version("${version}").as_bytes(),
        )],
    );

    let err = initialize_from_zip(&zip_path, temp.path().join("out"), "zh-CN").unwrap_err();

    assert!(err.to_string().contains("manifest.version"));
}

#[test]
fn init_writes_config_lists_and_common_overrides_for_empty_manifest() {
    let temp = tempfile::tempdir().unwrap();
    let zip_path = temp.path().join("pack.zip");
    let out = temp.path().join("project");
    support::write_zip(
        &zip_path,
        &[
            ("manifest.json", support::empty_manifest().as_bytes()),
            ("overrides/config/example.cfg", b"enabled=true".as_slice()),
        ],
    );

    initialize_from_zip_with_api(&zip_path, &out, "zh-CN", &support::FixtureApi).unwrap();

    let config = std::fs::read_to_string(out.join("config.toml")).unwrap();
    assert!(config.contains("name = \"ExamplePack-cleanroom\""));
    assert!(config.contains("loader = \"cleanroom\""));
    assert!(config.contains("[network]"));
    assert!(config.contains("proxy_ip = \"\""));
    assert!(config.contains("proxy_port = 0"));
    assert!(!config.contains("[curseforge]"));
    assert!(!config.contains("curseforge_api_key"));
    assert!(!config.lines().any(|line| line.trim() == "api_key = \"\""));
    assert!(config.contains("scalar_version = 2"));
    assert!(config.contains("[java.client]"));
    assert!(config.contains("[java.server]"));
    assert!(!config.contains("java_path"));
    assert!(!config.contains("CHANGE_ME_JAVA_PATH"));
    mcmpu::config::Config::from_path(out.join("config.toml")).unwrap();
    let common: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(out.join("common.json")).unwrap()).unwrap();
    let client: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(out.join("client.json")).unwrap()).unwrap();
    assert_eq!(common.as_array().unwrap(), &Vec::<serde_json::Value>::new());
    assert_eq!(client.as_array().unwrap(), &Vec::<serde_json::Value>::new());
    assert_eq!(
        std::fs::read_to_string(out.join("server.json")).unwrap(),
        "[]\n"
    );
    assert_eq!(
        std::fs::read_to_string(out.join("resourcepacks.json")).unwrap(),
        "[]\n"
    );
    assert_eq!(
        std::fs::read_to_string(out.join("overrides/common/config/example.cfg")).unwrap(),
        "enabled=true"
    );
    assert!(out.join("overrides/client").is_dir());
    assert!(out.join("overrides/server").is_dir());
}

#[test]
fn init_does_not_inject_cleanroom_supplements() {
    let temp = tempfile::tempdir().unwrap();
    let zip_path = temp.path().join("pack.zip");
    let out = temp.path().join("project");
    support::write_zip(
        &zip_path,
        &[
            (
                "manifest.json",
                manifest_with_files(
                    r#"[{"projectID": 1336201, "fileID": 7000000, "required": true}]"#,
                )
                .as_bytes(),
            ),
            ("overrides/config/example.cfg", b"enabled=true".as_slice()),
        ],
    );

    initialize_from_zip_with_api(&zip_path, &out, "zh-CN", &support::FixtureApi).unwrap();

    let client: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(out.join("client.json")).unwrap()).unwrap();
    let common: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(out.join("common.json")).unwrap()).unwrap();
    assert!(client.as_array().unwrap().iter().all(|entry| {
        entry["project_id"] != 1_214_490
            && entry["project_id"] != 1_214_906
            && entry["project_id"] != 1_336_201
    }));
    assert!(
        common
            .as_array()
            .unwrap()
            .iter()
            .all(|entry| entry["project_id"] != 1_005_815)
    );
}

#[test]
fn init_resolves_non_empty_manifest_and_splits_dependency_lists() {
    let temp = tempfile::tempdir().unwrap();
    let zip_path = temp.path().join("pack.zip");
    let out = temp.path().join("project");
    support::write_zip(
        &zip_path,
        &[
            (
                "manifest.json",
                manifest_with_files(
                    r#"[
                      {"projectID": 10, "fileID": 1010, "required": true},
                      {"projectID": 20, "fileID": 1020, "required": true},
                      {"projectID": 30, "fileID": 1030, "required": true},
                      {"projectID": 40, "fileID": 1040, "required": true}
                    ]"#,
                )
                .as_bytes(),
            ),
            ("overrides/config/example.cfg", b"enabled=true".as_slice()),
        ],
    );

    initialize_from_zip_with_api(&zip_path, &out, "zh-CN", &support::FixtureApi).unwrap();

    let common = read_json_array(out.join("common.json"));
    let client = read_json_array(out.join("client.json"));
    let server = read_json_array(out.join("server.json"));
    let resourcepacks = read_json_array(out.join("resourcepacks.json"));

    assert!(common.iter().any(|entry| entry["project_id"] == 10));
    assert!(client.iter().any(|entry| entry["project_id"] == 20));
    assert!(server.iter().any(|entry| entry["project_id"] == 30));
    assert!(resourcepacks.iter().any(|entry| entry["project_id"] == 40));
    assert!(common.iter().all(|entry| entry["project_id"] != 1_005_815));
    assert!(client.iter().all(|entry| entry["project_id"] != 1_214_906));
    assert!(common.iter().all(|entry| entry.get("side").is_none()));
    assert!(common.iter().all(|entry| entry.get("authors").is_none()));
}

#[test]
fn init_reports_progress_while_resolving_manifest_files() {
    let temp = tempfile::tempdir().unwrap();
    let zip_path = temp.path().join("pack.zip");
    let out = temp.path().join("project");
    support::write_zip(
        &zip_path,
        &[
            (
                "manifest.json",
                manifest_with_files(
                    r#"[
                      {"projectID": 10, "fileID": 1010, "required": true},
                      {"projectID": 20, "fileID": 1020, "required": true}
                    ]"#,
                )
                .as_bytes(),
            ),
            ("overrides/config/example.cfg", b"enabled=true".as_slice()),
        ],
    );
    let mut events = Vec::new();

    initialize_from_zip_with_api_and_progress(
        &zip_path,
        &out,
        "zh-CN",
        &support::FixtureApi,
        &mut |event| events.push(event.clone()),
    )
    .unwrap();

    assert!(
        events
            .iter()
            .any(|event| matches!(event, InitProgress::ReadingManifest))
    );
    let completed = events
        .iter()
        .filter_map(|event| match event {
            InitProgress::ResolvedFile {
                completed, total, ..
            } => Some((*completed, *total)),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(completed.len(), 2);
    assert!(completed.contains(&(1, 2)));
    assert!(completed.contains(&(2, 2)));
    assert!(
        events
            .iter()
            .any(|event| matches!(event, InitProgress::WritingProjectFiles { .. }))
    );
    assert!(
        events
            .iter()
            .any(|event| matches!(event, InitProgress::ExtractingOverrides { .. }))
    );
}

#[test]
fn init_rejects_non_empty_manifest_without_api_key() {
    let temp = tempfile::tempdir().unwrap();
    let zip_path = temp.path().join("pack.zip");
    support::write_zip(
        &zip_path,
        &[(
            "manifest.json",
            manifest_with_files(r#"[{"projectID": 10, "fileID": 1010, "required": true}]"#)
                .as_bytes(),
        )],
    );

    let err = initialize_from_zip(&zip_path, temp.path().join("out"), "zh-CN").unwrap_err();

    assert!(err.to_string().contains("CurseForge API key"));
}

#[test]
fn init_defaults_unknown_side_mods_to_common_and_writes_report() {
    let temp = tempfile::tempdir().unwrap();
    let zip_path = temp.path().join("pack.zip");
    let out = temp.path().join("project");
    support::write_zip(
        &zip_path,
        &[(
            "manifest.json",
            manifest_with_files(r#"[{"projectID": 60, "fileID": 1060, "required": true}]"#)
                .as_bytes(),
        )],
    );

    initialize_from_zip_with_api(&zip_path, &out, "zh-CN", &support::FixtureApi).unwrap();

    let common = read_json_array(out.join("common.json"));
    let client = read_json_array(out.join("client.json"));
    let report = std::fs::read_to_string(out.join("side-inference-report.txt")).unwrap();

    assert!(common.iter().any(|entry| entry["project_id"] == 60));
    assert!(client.iter().all(|entry| entry["project_id"] != 60));
    assert!(report.contains("Unknown Side Mod"));
    assert!(report.contains("60/1060"));
    assert!(report.contains("common"));
}

#[test]
fn init_infers_side_from_project_file_list_when_locked_file_has_no_side_tags() {
    let temp = tempfile::tempdir().unwrap();
    let zip_path = temp.path().join("pack.zip");
    let out = temp.path().join("project");
    support::write_zip(
        &zip_path,
        &[(
            "manifest.json",
            manifest_with_files(r#"[{"projectID": 70, "fileID": 1070, "required": true}]"#)
                .as_bytes(),
        )],
    );

    initialize_from_zip_with_api(&zip_path, &out, "zh-CN", &support::FixtureApi).unwrap();

    let common = read_json_array(out.join("common.json"));
    let report = std::fs::read_to_string(out.join("side-inference-report.txt")).unwrap();

    assert!(common.iter().any(|entry| entry["project_id"] == 70));
    assert!(!report.contains("Project Inferred Side Mod"));
    assert!(report.contains("所有依赖均已自动推测端侧"));
}

#[test]
fn init_reuses_project_file_list_requests_for_side_inference() {
    let temp = tempfile::tempdir().unwrap();
    let zip_path = temp.path().join("pack.zip");
    let out = temp.path().join("project");
    support::write_zip(
        &zip_path,
        &[(
            "manifest.json",
            manifest_with_files(
                r#"[
                  {"projectID": 70, "fileID": 1070, "required": true},
                  {"projectID": 70, "fileID": 1071, "required": true}
                ]"#,
            )
            .as_bytes(),
        )],
    );
    let api = CountingApi::default();

    initialize_from_zip_with_api(&zip_path, &out, "zh-CN", &api).unwrap();

    assert_eq!(api.project_calls(70), 1);
    assert_eq!(api.filtered_file_list_calls(70), 1);
    assert_eq!(api.project_file_list_calls(70), 1);
}

fn manifest_with_version(version: &str) -> String {
    format!(
        r#"{{
          "manifestType": "minecraftModpack",
          "manifestVersion": 1,
          "name": "ExamplePack-cleanroom",
          "version": "{version}",
          "author": "ExampleAuthor,SecondAuthor",
          "overrides": "overrides",
          "minecraft": {{
            "version": "1.12.2",
            "modLoaders": [{{"id": "forge-1.0.0", "primary": true}}]
          }},
          "files": []
        }}"#
    )
}

fn manifest_with_files(files: &str) -> String {
    format!(
        r#"{{
          "manifestType": "minecraftModpack",
          "manifestVersion": 1,
          "name": "ExamplePack-cleanroom",
          "version": "1.0.0",
          "author": "ExampleAuthor,SecondAuthor",
          "overrides": "overrides",
          "minecraft": {{
            "version": "1.12.2",
            "modLoaders": [{{"id": "forge-1.0.0", "primary": true}}]
          }},
          "files": {files}
        }}"#
    )
}

fn read_json_array(path: impl AsRef<std::path::Path>) -> Vec<serde_json::Value> {
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

#[derive(Default)]
struct CountingApi {
    project_calls: Mutex<HashMap<u64, usize>>,
    filtered_file_list_calls: Mutex<HashMap<u64, usize>>,
    project_file_list_calls: Mutex<HashMap<u64, usize>>,
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

    fn filtered_file_list_calls(&self, project_id: u64) -> usize {
        *self
            .filtered_file_list_calls
            .lock()
            .unwrap()
            .get(&project_id)
            .unwrap_or(&0)
    }

    fn project_file_list_calls(&self, project_id: u64) -> usize {
        *self
            .project_file_list_calls
            .lock()
            .unwrap()
            .get(&project_id)
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
        *self
            .filtered_file_list_calls
            .lock()
            .unwrap()
            .entry(project_id)
            .or_default() += 1;
        support::FixtureApi.list_files(project_id, minecraft_version, loader)
    }

    fn list_project_files(&self, project_id: u64) -> Result<Vec<CfFile>, ResolveError> {
        *self
            .project_file_list_calls
            .lock()
            .unwrap()
            .entry(project_id)
            .or_default() += 1;
        support::FixtureApi.list_project_files(project_id)
    }
}
