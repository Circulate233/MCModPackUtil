use std::cell::RefCell;
use std::collections::BTreeSet;

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
}

struct FakeApi {
    calls: RefCell<Vec<String>>,
    missing_download_url: bool,
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
