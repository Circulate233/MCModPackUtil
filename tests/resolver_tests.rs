use std::cell::RefCell;
use std::collections::BTreeSet;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Barrier};
use std::time::Duration;

use mcmpu::curseforge::cache::{CurseForgeCacheOptions, PersistentCurseForgeApi};
use mcmpu::curseforge::model::{CfFile, CfProject, ReleaseType};
use mcmpu::curseforge::resolver::{
    ApiKeyResolver, CandidateSelector, CurseForgeApi, DependencyResolver, ResolveError,
};
use mcmpu::dependency::{DependencyEntry, DependencySide, DependencyType};

#[test]
fn api_key_resolution_prefers_cli_then_env_then_builtin() {
    assert_eq!(
        ApiKeyResolver::resolve(Some(" cli-key "), Some("env-key"), "builtin-key").unwrap(),
        "cli-key"
    );
    assert_eq!(
        ApiKeyResolver::resolve(None, Some(" env-key "), "builtin-key").unwrap(),
        "env-key"
    );
    assert_eq!(
        ApiKeyResolver::resolve(None, None, " builtin-key ").unwrap(),
        "builtin-key"
    );

    let err = ApiKeyResolver::resolve(Some(" "), Some(""), " ").unwrap_err();
    assert!(err.to_string().contains("CurseForge API key"));
    assert!(err.to_string().contains("official build"));
}

#[test]
fn api_key_resolver_reads_common_environment_aliases() {
    unsafe {
        std::env::set_var("MCMPU_DISABLE_PERSISTENT_ENV", "1");
        std::env::set_var("MCMODPACKUTIL_CURSEFORGE_API_KEY", " ");
        std::env::set_var("CURSEFORGE_API_KEY", " alias-key ");
        std::env::remove_var("CF_API_KEY");
    }

    assert_eq!(ApiKeyResolver::env_key().as_deref(), Some("alias-key"));

    unsafe {
        std::env::remove_var("MCMPU_DISABLE_PERSISTENT_ENV");
        std::env::remove_var("MCMODPACKUTIL_CURSEFORGE_API_KEY");
        std::env::remove_var("CURSEFORGE_API_KEY");
    }
}

#[test]
fn api_key_resolver_reads_compile_time_builtin_key() {
    if cfg!(debug_assertions) {
        assert_eq!(ApiKeyResolver::builtin_key(), None);
    } else {
        assert_eq!(
            ApiKeyResolver::builtin_key().as_deref(),
            option_env!("MCMPU_BUILTIN_CURSEFORGE_API_KEY")
                .map(str::trim)
                .filter(|value| !value.is_empty())
        );
    }
}

#[test]
fn candidate_selector_prefers_stable_then_beta_and_rejects_alpha() {
    let stable = cf_file(10, 100, ReleaseType::Stable, Some("https://cdn/stable.jar"));
    let beta = cf_file(11, 100, ReleaseType::Beta, Some("https://cdn/beta.jar"));
    let alpha = cf_file(12, 100, ReleaseType::Alpha, Some("https://cdn/alpha.jar"));

    assert_eq!(
        CandidateSelector::select_latest(&[beta.clone(), stable.clone()])
            .unwrap()
            .id,
        10
    );
    assert_eq!(
        CandidateSelector::select_latest(std::slice::from_ref(&beta))
            .unwrap()
            .id,
        11
    );

    let err = CandidateSelector::select_latest(&[alpha]).unwrap_err();
    assert!(err.to_string().contains("alpha"));
}

#[test]
fn persistent_cache_reuses_project_and_fixed_file_across_api_instances() {
    let temp = tempfile::tempdir().unwrap();
    let api = PersistentCountingApi::default();

    let first = PersistentCurseForgeApi::new(&api, Some(temp.path()));
    assert_eq!(first.get_project(100).unwrap().id, 100);
    assert_eq!(first.get_file(100, 200).unwrap().id, 200);

    let second = PersistentCurseForgeApi::new(&api, Some(temp.path()));
    assert_eq!(second.get_project(100).unwrap().id, 100);
    assert_eq!(second.get_file(100, 200).unwrap().id, 200);

    assert_eq!(api.project_calls.load(Ordering::SeqCst), 1);
    assert_eq!(api.file_calls.load(Ordering::SeqCst), 1);

    let records = std::fs::read_dir(temp.path().join(".mcmpu/cache/curseforge-api"))
        .unwrap()
        .map(|entry| {
            serde_json::from_slice::<serde_json::Value>(
                &std::fs::read(entry.unwrap().path()).unwrap(),
            )
            .unwrap()
        })
        .collect::<Vec<_>>();
    assert_eq!(records.len(), 2);
    for record in &records {
        assert_eq!(record["version"], 1);
        assert!(record["fetched_at"].is_u64());
        assert!(record["request"]["method"].is_string());
        assert!(record.get("value").is_some());
        assert_eq!(record.as_object().unwrap().len(), 4);
        assert!(!record.to_string().contains("api_key"));
    }
    assert!(records.iter().any(|record| {
        record["request"]["method"] == "get_project"
            && record["request"]["parameters"]["project_id"] == 100
    }));
    assert!(records.iter().any(|record| {
        record["request"]["method"] == "get_file"
            && record["request"]["parameters"]["project_id"] == 100
            && record["request"]["parameters"]["file_id"] == 200
    }));
}

#[test]
fn persistent_cache_refetches_expired_records_and_uses_short_file_list_ttl() {
    let temp = tempfile::tempdir().unwrap();
    let api = PersistentCountingApi::default();
    let defaults = CurseForgeCacheOptions::default();
    assert_eq!(defaults.project_ttl, Duration::from_secs(24 * 60 * 60));
    assert_eq!(defaults.file_ttl, Duration::from_secs(24 * 60 * 60));
    assert_eq!(defaults.file_list_ttl, Duration::from_secs(10 * 60));

    let first = PersistentCurseForgeApi::with_options(&api, Some(temp.path()), defaults);
    first.get_project(100).unwrap();
    first.list_files(100, "1.12.2", "forge").unwrap();

    let cached = PersistentCurseForgeApi::with_options(&api, Some(temp.path()), defaults);
    cached.get_project(100).unwrap();
    cached.list_files(100, "1.12.2", "forge").unwrap();
    assert_eq!(api.project_calls.load(Ordering::SeqCst), 1);
    assert_eq!(api.list_calls.load(Ordering::SeqCst), 1);

    let expired = CurseForgeCacheOptions {
        project_ttl: Duration::ZERO,
        file_list_ttl: Duration::ZERO,
        ..defaults
    };
    let refreshed = PersistentCurseForgeApi::with_options(&api, Some(temp.path()), expired);
    refreshed.get_project(100).unwrap();
    refreshed.list_files(100, "1.12.2", "forge").unwrap();
    assert_eq!(api.project_calls.load(Ordering::SeqCst), 2);
    assert_eq!(api.list_calls.load(Ordering::SeqCst), 2);
}

#[test]
fn persistent_cache_deduplicates_concurrent_requests_across_instances() {
    let temp = tempfile::tempdir().unwrap();
    let api = Arc::new(PersistentCountingApi::default());
    let barrier = Arc::new(Barrier::new(8));

    std::thread::scope(|scope| {
        for _ in 0..8 {
            let api = Arc::clone(&api);
            let barrier = Arc::clone(&barrier);
            let root = temp.path().to_path_buf();
            scope.spawn(move || {
                barrier.wait();
                let cached = PersistentCurseForgeApi::new(api.as_ref(), Some(&root));
                assert_eq!(cached.get_project(100).unwrap().id, 100);
            });
        }
    });

    assert_eq!(api.project_calls.load(Ordering::SeqCst), 1);
}

#[test]
fn persistent_cache_deduplicates_requests_across_processes() {
    let temp = tempfile::tempdir().unwrap();
    let gate = temp.path().join("gate");
    let calls = temp.path().join("calls");
    let executable = std::env::current_exe().unwrap();
    let mut children = Vec::new();
    for _ in 0..2 {
        children.push(
            std::process::Command::new(&executable)
                .args([
                    "--exact",
                    "persistent_cache_cross_process_fixture",
                    "--nocapture",
                ])
                .env("MCMPU_CF_CACHE_FIXTURE_ROOT", temp.path())
                .env("MCMPU_CF_CACHE_FIXTURE_GATE", &gate)
                .env("MCMPU_CF_CACHE_FIXTURE_CALLS", &calls)
                .spawn()
                .unwrap(),
        );
    }
    std::fs::write(&gate, b"go").unwrap();
    for child in &mut children {
        assert!(child.wait().unwrap().success());
    }
    let call_lines = std::fs::read_to_string(calls).unwrap();
    assert_eq!(call_lines.lines().count(), 1);
}

#[test]
fn persistent_cache_cross_process_fixture() {
    let Some(root) = std::env::var_os("MCMPU_CF_CACHE_FIXTURE_ROOT") else {
        return;
    };
    let gate = std::path::PathBuf::from(
        std::env::var_os("MCMPU_CF_CACHE_FIXTURE_GATE").expect("fixture gate"),
    );
    let calls = std::path::PathBuf::from(
        std::env::var_os("MCMPU_CF_CACHE_FIXTURE_CALLS").expect("fixture calls"),
    );
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while !gate.is_file() && std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(gate.is_file());
    let api = CrossProcessCountingApi { calls };
    let cached = PersistentCurseForgeApi::new(&api, Some(std::path::Path::new(&root)));
    assert_eq!(cached.get_project(100).unwrap().id, 100);
}

#[test]
fn persistent_cache_reports_corrupt_record_path_without_refetching() {
    let temp = tempfile::tempdir().unwrap();
    let api = PersistentCountingApi::default();
    PersistentCurseForgeApi::new(&api, Some(temp.path()))
        .get_project(100)
        .unwrap();
    let cache_dir = temp.path().join(".mcmpu/cache/curseforge-api");
    let cache_path = std::fs::read_dir(&cache_dir)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| {
            path.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("get-project-")
        })
        .unwrap();
    std::fs::write(&cache_path, b"{not valid json").unwrap();

    let error = PersistentCurseForgeApi::new(&api, Some(temp.path()))
        .get_project(100)
        .unwrap_err();
    let canonical_cache_path = std::fs::canonicalize(&cache_path).unwrap();

    assert!(error.to_string().contains("failed to parse cache record"));
    assert!(
        error
            .to_string()
            .contains(&canonical_cache_path.display().to_string())
    );
    assert_eq!(api.project_calls.load(Ordering::SeqCst), 1);
}

#[test]
fn persistent_cache_rejects_future_timestamp_without_refetching() {
    let temp = tempfile::tempdir().unwrap();
    let api = PersistentCountingApi::default();
    PersistentCurseForgeApi::new(&api, Some(temp.path()))
        .get_project(100)
        .unwrap();
    let cache_path = std::fs::read_dir(temp.path().join(".mcmpu/cache/curseforge-api"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| {
            path.extension()
                .is_some_and(|extension| extension == "json")
        })
        .unwrap();
    let mut record: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&cache_path).unwrap()).unwrap();
    record["fetched_at"] = serde_json::Value::from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs()
            + 3600,
    );
    std::fs::write(&cache_path, serde_json::to_vec_pretty(&record).unwrap()).unwrap();

    let error = PersistentCurseForgeApi::new(&api, Some(temp.path()))
        .get_project(100)
        .unwrap_err();

    assert!(error.to_string().contains("is in the future"));
    assert_eq!(api.project_calls.load(Ordering::SeqCst), 1);
}

#[test]
fn persistent_cache_internal_disable_and_refresh_options_bypass_existing_records() {
    let temp = tempfile::tempdir().unwrap();
    let api = PersistentCountingApi::default();
    PersistentCurseForgeApi::new(&api, Some(temp.path()))
        .get_project(100)
        .unwrap();
    PersistentCurseForgeApi::with_options(
        &api,
        Some(temp.path()),
        CurseForgeCacheOptions::refresh(),
    )
    .get_project(100)
    .unwrap();
    PersistentCurseForgeApi::with_options(
        &api,
        Some(temp.path()),
        CurseForgeCacheOptions::disabled(),
    )
    .get_project(100)
    .unwrap();

    assert_eq!(api.project_calls.load(Ordering::SeqCst), 3);
}

#[test]
fn resolver_uses_locked_project_and_file_ids_before_slug() {
    let api = FakeApi::new();
    let resolver = DependencyResolver::new(api);
    let entry = DependencyEntry {
        project_id: Some(100),
        file_id: Some(200),
        slug: Some("red-core".to_string()),
        loader: Some("forge".to_string()),
        side: DependencySide::Common,
        dependency_type: DependencyType::Mod,
        name: "Red Core".to_string(),
        authors: vec!["Desoroxxx".to_string()],
        required: true,
    };

    let resolved = resolver.resolve_entry(&entry, "1.12.2", "forge").unwrap();

    assert_eq!(resolved.project.id, 100);
    assert_eq!(resolved.file.id, 200);
    assert_eq!(
        resolver.api().calls.borrow().as_slice(),
        ["project:100", "file:100:200"]
    );
}

#[test]
fn resolver_omits_loader_filter_when_entry_loader_is_null() {
    let api = FakeApi::new();
    let resolver = DependencyResolver::new(api);
    let entry = DependencyEntry {
        project_id: None,
        file_id: None,
        slug: Some("skin-layers-3d".to_string()),
        loader: None,
        side: DependencySide::Client,
        dependency_type: DependencyType::Mod,
        name: "Skin Layers 3D".to_string(),
        authors: vec!["tr7zw".to_string()],
        required: true,
    };

    let resolved = resolver
        .resolve_entry(&entry, "1.21.1", "neoforge")
        .unwrap();

    assert_eq!(resolved.project.slug, "skin-layers-3d");
    assert_eq!(resolved.file.id, 301);
    assert_eq!(
        resolver.api().calls.borrow().as_slice(),
        ["slug:skin-layers-3d", "files:300:1.21.1:"]
    );
}

#[test]
fn resolver_uses_explicit_entry_loader_filter() {
    let api = FakeApi::new();
    let resolver = DependencyResolver::new(api);
    let entry = DependencyEntry {
        project_id: None,
        file_id: None,
        slug: Some("skin-layers-3d".to_string()),
        loader: Some("forge".to_string()),
        side: DependencySide::Client,
        dependency_type: DependencyType::Mod,
        name: "Skin Layers 3D".to_string(),
        authors: vec!["tr7zw".to_string()],
        required: true,
    };

    let resolved = resolver
        .resolve_entry(&entry, "1.21.1", "neoforge")
        .unwrap();

    assert_eq!(resolved.project.slug, "skin-layers-3d");
    assert_eq!(resolved.file.id, 301);
    assert_eq!(
        resolver.api().calls.borrow().as_slice(),
        ["slug:skin-layers-3d", "files:300:1.21.1:forge"]
    );
}

#[test]
fn resolver_rejects_files_without_download_url() {
    let api = FakeApi::new_without_download_url();
    let resolver = DependencyResolver::new(api);
    let entry = DependencyEntry {
        project_id: Some(100),
        file_id: Some(200),
        slug: Some("red-core".to_string()),
        loader: Some("forge".to_string()),
        side: DependencySide::Common,
        dependency_type: DependencyType::Mod,
        name: "Red Core".to_string(),
        authors: vec!["Desoroxxx".to_string()],
        required: true,
    };

    let err = resolver
        .resolve_entry(&entry, "1.12.2", "forge")
        .unwrap_err();

    assert!(err.to_string().contains("download URL"));
    assert!(
        err.to_string()
            .contains("https://www.curseforge.com/minecraft/mc-mods/red-core/files/200")
    );
}

struct FakeApi {
    calls: RefCell<Vec<String>>,
    missing_download_url: bool,
}

#[derive(Default)]
struct PersistentCountingApi {
    project_calls: AtomicUsize,
    file_calls: AtomicUsize,
    list_calls: AtomicUsize,
}

struct CrossProcessCountingApi {
    calls: std::path::PathBuf,
}

impl CurseForgeApi for CrossProcessCountingApi {
    fn get_project(&self, project_id: u64) -> Result<CfProject, ResolveError> {
        use std::io::Write as _;
        let mut calls = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.calls)
            .unwrap();
        writeln!(calls, "{project_id}").unwrap();
        calls.sync_all().unwrap();
        std::thread::sleep(Duration::from_millis(250));
        Ok(CfProject::new_for_test(
            project_id,
            "Red Core",
            "red-core",
            6,
            vec!["Desoroxxx"],
        ))
    }

    fn get_file(&self, _project_id: u64, _file_id: u64) -> Result<CfFile, ResolveError> {
        unreachable!("fixture only requests a project")
    }

    fn find_project_by_slug(&self, _slug: &str) -> Result<CfProject, ResolveError> {
        unreachable!("fixture only requests a project")
    }

    fn list_files(
        &self,
        _project_id: u64,
        _minecraft_version: &str,
        _loader: &str,
    ) -> Result<Vec<CfFile>, ResolveError> {
        unreachable!("fixture only requests a project")
    }
}

impl CurseForgeApi for PersistentCountingApi {
    fn get_project(&self, project_id: u64) -> Result<CfProject, ResolveError> {
        self.project_calls.fetch_add(1, Ordering::SeqCst);
        std::thread::sleep(Duration::from_millis(20));
        Ok(CfProject::new_for_test(
            project_id,
            "Red Core",
            "red-core",
            6,
            vec!["Desoroxxx"],
        ))
    }

    fn get_file(&self, project_id: u64, file_id: u64) -> Result<CfFile, ResolveError> {
        self.file_calls.fetch_add(1, Ordering::SeqCst);
        Ok(cf_file(
            file_id,
            project_id,
            ReleaseType::Stable,
            Some("https://cdn/red-core.jar"),
        ))
    }

    fn find_project_by_slug(&self, slug: &str) -> Result<CfProject, ResolveError> {
        Ok(CfProject::new_for_test(
            100,
            "Red Core",
            slug,
            6,
            vec!["Desoroxxx"],
        ))
    }

    fn list_files(
        &self,
        project_id: u64,
        _minecraft_version: &str,
        _loader: &str,
    ) -> Result<Vec<CfFile>, ResolveError> {
        self.list_calls.fetch_add(1, Ordering::SeqCst);
        Ok(vec![cf_file(
            200,
            project_id,
            ReleaseType::Stable,
            Some("https://cdn/red-core.jar"),
        )])
    }
}

impl FakeApi {
    fn new() -> Self {
        Self {
            calls: RefCell::new(Vec::new()),
            missing_download_url: false,
        }
    }

    fn new_without_download_url() -> Self {
        Self {
            calls: RefCell::new(Vec::new()),
            missing_download_url: true,
        }
    }
}

impl CurseForgeApi for FakeApi {
    fn get_project(&self, project_id: u64) -> Result<CfProject, ResolveError> {
        self.calls
            .borrow_mut()
            .push(format!("project:{project_id}"));
        Ok(CfProject::new_for_test(
            project_id,
            "Red Core",
            "red-core",
            6,
            vec!["Desoroxxx"],
        ))
    }

    fn get_file(&self, project_id: u64, file_id: u64) -> Result<CfFile, ResolveError> {
        self.calls
            .borrow_mut()
            .push(format!("file:{project_id}:{file_id}"));
        Ok(cf_file(
            file_id,
            project_id,
            ReleaseType::Stable,
            (!self.missing_download_url).then_some("https://cdn/red-core.jar"),
        ))
    }

    fn find_project_by_slug(&self, slug: &str) -> Result<CfProject, ResolveError> {
        self.calls.borrow_mut().push(format!("slug:{slug}"));
        Ok(CfProject::new_for_test(
            300,
            "Skin Layers 3D",
            slug,
            6,
            vec!["tr7zw"],
        ))
    }

    fn list_files(
        &self,
        project_id: u64,
        minecraft_version: &str,
        loader: &str,
    ) -> Result<Vec<CfFile>, ResolveError> {
        self.calls
            .borrow_mut()
            .push(format!("files:{project_id}:{minecraft_version}:{loader}"));
        Ok(vec![
            cf_file(
                302,
                project_id,
                ReleaseType::Beta,
                Some("https://cdn/beta.jar"),
            ),
            cf_file(
                301,
                project_id,
                ReleaseType::Stable,
                Some("https://cdn/stable.jar"),
            ),
        ])
    }
}

fn cf_file(id: u64, mod_id: u64, release_type: ReleaseType, download_url: Option<&str>) -> CfFile {
    CfFile {
        id,
        mod_id,
        file_name: format!("file-{id}.jar"),
        display_name: None,
        file_length: 42,
        is_available: true,
        file_status: 4,
        release_type,
        game_versions: BTreeSet::from([
            "Client".to_string(),
            "Server".to_string(),
            "Forge".to_string(),
            "1.12.2".to_string(),
        ]),
        dependencies: vec![],
        download_url: download_url.map(ToOwned::to_owned),
        hashes: vec![],
    }
}
