use std::collections::HashMap;
use std::io::Read;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use mcmpu::curseforge::model::{CfFile, CfProject};
use mcmpu::curseforge::resolver::{CurseForgeApi, ResolveError};
use mcmpu::dependency::{DependencyEntry, DependencySide, DependencyType};
use mcmpu::project::build::{
    BuildProgress, build_project_with_api, build_project_with_api_and_progress,
    validate_project_with_api,
};
use zip::ZipArchive;

mod support;

#[test]
fn build_writes_client_and_server_zip_outputs() {
    let temp = tempfile::tempdir().unwrap();
    let project = temp.path();
    support::write_build_project(project);

    let outputs = build_project_with_api(project, &support::FixtureApi).unwrap();

    assert_eq!(
        outputs.client_zip.file_name().unwrap().to_string_lossy(),
        "ExamplePack-cleanroom.zip"
    );
    assert_eq!(
        outputs.server_zip.file_name().unwrap().to_string_lossy(),
        "ExamplePack-cleanroom-1.0.0-server.zip"
    );

    let mut client = open_zip(&outputs.client_zip);
    assert!(zip_has(&mut client, "manifest.json"));
    assert!(zip_has(&mut client, "modlist.html"));
    assert!(zip_has(&mut client, "overrides/config/common.json"));
    assert!(zip_has(&mut client, "overrides/options.txt"));
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
    assert!(zip_has(&mut server, "start.bat"));
    assert!(zip_has(&mut server, "start.sh"));
    assert!(zip_has(&mut server, "mcmpu-download.ps1"));
    assert!(zip_has(&mut server, "server-files.json"));
    assert!(zip_has(&mut server, "config/common.json"));
    assert!(zip_has(&mut server, "server.properties"));
    assert!(!zip_has(&mut server, "overrides/config/common.json"));
    assert!(!zip_has(&mut server, "overrides/server.properties"));
    assert!(zip_has(&mut server, "resourcepacks/.mcmpu-keep"));
    assert!(zip_has(&mut server, "README.md"));
    assert!(zip_has(&mut server, "LICENSE"));

    let server_files: serde_json::Value = read_zip_json(&mut server, "server-files.json");
    let files = server_files["files"].as_array().unwrap();
    assert!(
        files
            .iter()
            .any(|file| file["path"] == "mods/common-mod-10-1010.jar")
    );
    assert!(
        files
            .iter()
            .any(|file| file["path"] == "mods/server-mod-30-1030.jar")
    );
    assert!(
        files
            .iter()
            .any(|file| file["path"] == "resourcepacks/resource-pack-40-1040.zip")
    );
    assert!(
        !files
            .iter()
            .any(|file| file["path"] == "mods/client-mod-20-1020.jar")
    );
    assert!(files.iter().all(|file| {
        file["url"]
            .as_str()
            .unwrap()
            .contains("cdn.example.invalid")
    }));

    let start_bat = read_zip_text(&mut server, "start.bat");
    let start_sh = read_zip_text(&mut server, "start.sh");
    let download_ps1 = read_zip_text(&mut server, "mcmpu-download.ps1");
    assert!(start_bat.contains("set \"CF_API_KEY=test-build-api-key\""));
    assert!(start_bat.contains("setlocal DisableDelayedExpansion"));
    assert!(start_bat.contains("pause"));
    assert!(start_sh.contains("sha1sum"));
    assert!(start_sh.contains("CF_API_KEY='test-build-api-key'"));
    assert!(start_sh.contains("--installServer"));
    assert!(download_ps1.contains("x-api-key"));
    assert!(!start_bat.contains("cursemaven"));
    assert!(!start_sh.contains("cursemaven"));
    assert!(!download_ps1.contains("cursemaven"));
    assert!(!server_files.to_string().contains("cursemaven"));
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

    let outputs = build_project_with_api(project, &support::FixtureApi).unwrap();
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

    build_project_with_api(project, &api).unwrap();

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
    let outputs = build_project_with_api(project, &support::FixtureApi).unwrap();

    let mut server = open_zip(&outputs.server_zip);
    let server_files: serde_json::Value = read_zip_json(&mut server, "server-files.json");
    let files = server_files["files"].as_array().unwrap();

    assert!(files.iter().any(|file| {
        file["path"] == "mods/server-mod-30-1030.jar"
            && file["url"] == "https://cdn.example.invalid/30/1030/server-mod.jar"
    }));
}

#[test]
fn build_server_fails_when_curseforge_download_url_is_missing() {
    let temp = tempfile::tempdir().unwrap();
    let project = temp.path();
    support::write_build_project(project);

    let err = build_project_with_api(project, &MissingServerDownloadUrlApi).unwrap_err();

    assert!(err.to_string().contains("download URL"));
}

#[test]
fn build_does_not_block_on_curseforge_required_dependency_metadata() {
    let temp = tempfile::tempdir().unwrap();
    let project = temp.path();
    support::write_build_project(project);

    let outputs = build_project_with_api(project, &RequiredDependencyApi).unwrap();

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

    let outputs = build_project_with_api(project, &support::FixtureApi).unwrap();

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

    let outputs = build_project_with_api(project, &support::FixtureApi).unwrap();
    let mut client = open_zip(&outputs.client_zip);
    let manifest: serde_json::Value = read_zip_json(&mut client, "manifest.json");
    let mut server = open_zip(&outputs.server_zip);
    let server_files: serde_json::Value = read_zip_json(&mut server, "server-files.json");

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
            .any(|file| file["path"] == "mods/unknown-side-mod-60-1060.jar")
    );
}

#[test]
fn build_reports_progress_while_resolving_and_writing_outputs() {
    let temp = tempfile::tempdir().unwrap();
    let project = temp.path();
    support::write_build_project(project);
    let mut events = Vec::new();

    build_project_with_api_and_progress(project, &support::FixtureApi, &mut |event| {
        events.push(event.clone());
    })
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
    assert!(events.iter().any(|event| {
        matches!(event, BuildProgress::WritingClientZip { path } if path.ends_with("ExamplePack-cleanroom.zip"))
    }));
    assert!(events.iter().any(|event| {
        matches!(
            event,
            BuildProgress::WritingServerZip { path } if path.ends_with("-server.zip")
        )
    }));
    assert!(matches!(
        events.last(),
        Some(BuildProgress::Finished {
            client_zip,
            server_zip
        }) if client_zip.ends_with("ExamplePack-cleanroom.zip") && server_zip.ends_with("-server.zip")
    ));
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

struct MissingServerDownloadUrlApi;

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

impl CurseForgeApi for MissingServerDownloadUrlApi {
    fn get_project(&self, project_id: u64) -> Result<CfProject, ResolveError> {
        support::FixtureApi.get_project(project_id)
    }

    fn get_file(&self, project_id: u64, file_id: u64) -> Result<CfFile, ResolveError> {
        let mut file = support::FixtureApi.get_file(project_id, file_id)?;
        if project_id == 30 {
            file.download_url = None;
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
