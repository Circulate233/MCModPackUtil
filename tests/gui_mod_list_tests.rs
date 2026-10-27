mod support;

use std::cell::RefCell;
use std::path::Path;
use std::sync::{Arc, Barrier, Mutex, MutexGuard, OnceLock};

use mcmpu::curseforge::model::{CfFile, CfProject};
use mcmpu::curseforge::resolver::{CurseForgeApi, ResolveError};
use mcmpu::dependency::{DependencySide, DependencyType};
use mcmpu::gui::mod_list::{
    AddProjectDraft, GuiDependencyList, GuiDependencyType, GuiModEntry, ManifestList,
    ModListProjectConfig, ModListValidationError, ProjectLookup, ProjectSearchInput,
    QueryFileCandidateCache, QueryFileCandidates, load_project_config_for_mod_list,
    parse_project_search_input, query_file_candidates, query_file_candidates_cached,
    search_project_candidates,
};

#[test]
fn loads_four_manifests_grouped_by_dependency_type() {
    let temp = tempfile::tempdir().unwrap();
    write_mod_list_project(temp.path());

    let list = GuiDependencyList::load(temp.path()).unwrap();

    assert_eq!(
        list.mods()
            .iter()
            .map(|entry| entry.name.as_str())
            .collect::<Vec<_>>(),
        ["Common Mod", "Client Mod", "Server Mod"]
    );
    assert_eq!(
        list.resource_packs()
            .iter()
            .map(|entry| (entry.name.as_str(), entry.manifest))
            .collect::<Vec<_>>(),
        [("Resource Pack", ManifestList::Resourcepacks)]
    );
    assert_eq!(
        list.shader_packs()
            .iter()
            .map(|entry| (entry.name.as_str(), entry.manifest))
            .collect::<Vec<_>>(),
        [("Shader Pack", ManifestList::Resourcepacks)]
    );
}

#[test]
fn missing_manifest_files_open_as_an_empty_gui_list() {
    let temp = tempfile::tempdir().unwrap();

    let list = GuiDependencyList::load(temp.path()).unwrap();

    assert!(list.entries.is_empty());
    assert_eq!(list.contaminated_manifests().count(), 0);
}

#[test]
fn malformed_manifest_does_not_hide_valid_entries_from_other_manifests() {
    let temp = tempfile::tempdir().unwrap();
    let common_path = temp.path().join("common.json");
    let original = b"[{not valid json]".to_vec();
    std::fs::write(&common_path, &original).unwrap();
    support::write_entries(
        temp.path().join("client.json"),
        &[named_entry(
            20,
            1020,
            DependencySide::Client,
            "Client Mod",
            DependencyType::Mod,
        )],
    );

    let list = GuiDependencyList::load(temp.path()).unwrap();

    assert_eq!(list.entries.len(), 1);
    assert_eq!(list.entries[0].name, "Client Mod");
    assert_eq!(list.entries[0].manifest, ManifestList::Client);
    assert_eq!(
        list.contaminated_manifests().collect::<Vec<_>>(),
        [ManifestList::Common]
    );

    let error = list.save(temp.path()).unwrap_err();
    assert!(matches!(
        error,
        mcmpu::gui::mod_list::ModListIoError::ContaminatedManifests { ref files }
            if files == "common.json"
    ));
    assert_eq!(std::fs::read(common_path).unwrap(), original);
}

#[test]
fn mixed_manifest_skips_invalid_entries_and_keeps_valid_entries() {
    let temp = tempfile::tempdir().unwrap();
    let valid = serde_json::to_value(named_entry(
        10,
        1010,
        DependencySide::Common,
        "Common Mod",
        DependencyType::Mod,
    ))
    .unwrap();
    let structurally_invalid = serde_json::json!({
        "project_id": "not-a-number",
        "file_id": 2020,
        "slug": "broken",
        "loader": "forge",
        "type": "mod",
        "name": "Broken Mod",
        "required": true
    });
    let semantically_invalid = serde_json::json!({
        "project_id": null,
        "file_id": null,
        "slug": null,
        "loader": "forge",
        "type": "mod",
        "name": "Missing Locator",
        "required": true
    });
    let common_path = temp.path().join("common.json");
    let original = serde_json::to_vec_pretty(&serde_json::json!([
        valid,
        structurally_invalid,
        semantically_invalid
    ]))
    .unwrap();
    std::fs::write(&common_path, &original).unwrap();

    let list = GuiDependencyList::load(temp.path()).unwrap();

    assert_eq!(list.entries.len(), 1);
    assert_eq!(list.entries[0].name, "Common Mod");
    assert!(list.is_manifest_contaminated(ManifestList::Common));

    assert!(matches!(
        list.save(temp.path()).unwrap_err(),
        mcmpu::gui::mod_list::ModListIoError::ContaminatedManifests { .. }
    ));
    assert_eq!(std::fs::read(common_path).unwrap(), original);
}

#[test]
fn unknown_entry_fields_contaminate_the_manifest_and_block_save() {
    let temp = tempfile::tempdir().unwrap();
    let common_path = temp.path().join("common.json");
    let valid = serde_json::to_value(named_entry(
        10,
        1010,
        DependencySide::Common,
        "Common Mod",
        DependencyType::Mod,
    ))
    .unwrap();
    let mut unsupported = serde_json::to_value(named_entry(
        20,
        1020,
        DependencySide::Common,
        "Future Mod",
        DependencyType::Mod,
    ))
    .unwrap();
    unsupported
        .as_object_mut()
        .unwrap()
        .insert("future_field".to_string(), serde_json::json!(true));
    let original = serde_json::to_vec_pretty(&serde_json::json!([valid, unsupported])).unwrap();
    std::fs::write(&common_path, &original).unwrap();

    let list = GuiDependencyList::load(temp.path()).unwrap();

    assert_eq!(list.entries.len(), 1);
    assert_eq!(list.entries[0].name, "Common Mod");
    assert!(list.is_manifest_contaminated(ManifestList::Common));
    assert!(matches!(
        list.save(temp.path()).unwrap_err(),
        mcmpu::gui::mod_list::ModListIoError::ContaminatedManifests { .. }
    ));
    assert_eq!(std::fs::read(common_path).unwrap(), original);
}

#[test]
fn tolerant_loading_does_not_weaken_save_validation() {
    let temp = tempfile::tempdir().unwrap();
    write_mod_list_project(temp.path());
    let common_path = temp.path().join("common.json");
    let original = std::fs::read(&common_path).unwrap();
    let mut list = GuiDependencyList::load(temp.path()).unwrap();
    list.entries[0].project_id = None;
    list.entries[0].slug = None;

    let error = list.save(temp.path()).unwrap_err();

    assert!(matches!(
        error,
        mcmpu::gui::mod_list::ModListIoError::Validation(
            ModListValidationError::MissingProjectLocator { index: 0 }
        )
    ));
    assert_eq!(std::fs::read(common_path).unwrap(), original);
}

#[test]
fn save_preflight_failure_leaves_all_regular_manifests_unchanged() {
    let temp = tempfile::tempdir().unwrap();
    write_mod_list_project(temp.path());
    let mut list = GuiDependencyList::load(temp.path()).unwrap();
    list.entries[0].slug = Some("edited-but-not-written".to_string());

    let regular_paths = ["common.json", "client.json", "server.json"];
    let originals =
        regular_paths.map(|file_name| std::fs::read(temp.path().join(file_name)).unwrap());
    let resourcepacks = temp.path().join("resourcepacks.json");
    std::fs::remove_file(&resourcepacks).unwrap();
    std::fs::create_dir(&resourcepacks).unwrap();

    let error = list.save(temp.path()).unwrap_err();

    assert!(matches!(
        error,
        mcmpu::gui::mod_list::ModListIoError::Transaction { .. }
    ));
    for (file_name, original) in regular_paths.into_iter().zip(originals) {
        assert_eq!(
            std::fs::read(temp.path().join(file_name)).unwrap(),
            original
        );
    }
    assert!(resourcepacks.is_dir());
    let transaction_artifacts = std::fs::read_dir(temp.path())
        .unwrap()
        .filter_map(Result::ok)
        .filter_map(|entry| entry.file_name().into_string().ok())
        .filter(|name| name.starts_with('.') && (name.ends_with(".tmp") || name.ends_with(".bak")))
        .collect::<Vec<_>>();
    assert!(transaction_artifacts.is_empty());
}

#[test]
fn saves_entries_back_to_their_manifest_files_as_pretty_json() {
    let temp = tempfile::tempdir().unwrap();
    write_mod_list_project(temp.path());
    let mut list = GuiDependencyList::load(temp.path()).unwrap();

    list.entries[0].slug = Some("common-mod-edited".to_string());
    list.entries.push(GuiModEntry {
        manifest: ManifestList::Client,
        name: "New Client Mod".to_string(),
        dependency_type: GuiDependencyType::Mod,
        loader: Some("fabric".to_string()),
        project_id: Some(90),
        file_id: Some(1090),
        auto_update: false,
        slug: Some("new-client-mod".to_string()),
        required: false,
    });
    list.entries
        .retain(|entry| entry.manifest != ManifestList::Server);

    list.save(temp.path()).unwrap();

    let common = read_json(temp.path().join("common.json"));
    let client = read_json(temp.path().join("client.json"));
    let server = read_json(temp.path().join("server.json"));
    let resourcepacks = read_json(temp.path().join("resourcepacks.json"));

    assert_eq!(common[0]["name"], "Common Mod");
    assert_eq!(common[0]["slug"], "common-mod-edited");
    assert_eq!(client.as_array().unwrap().len(), 2);
    assert_eq!(client[1]["file_id"], 1090);
    assert_eq!(client[1]["required"], false);
    assert_eq!(server.as_array().unwrap().len(), 0);
    assert_eq!(resourcepacks.as_array().unwrap().len(), 2);
    assert!(
        std::fs::read_to_string(temp.path().join("client.json"))
            .unwrap()
            .contains("\n  {")
    );
}

#[test]
fn auto_update_entries_save_file_id_as_null_and_locked_entries_require_file_id() {
    let mut entry = valid_gui_entry();
    entry.file_id = Some(1234);
    entry.set_auto_update(true);
    assert_eq!(entry.file_id, None);
    assert!(!entry.allows_file_version_selection());
    entry.validate().unwrap();

    entry.set_auto_update(false);
    assert!(entry.allows_file_version_selection());
    assert!(matches!(
        entry.validate().unwrap_err(),
        ModListValidationError::MissingFileId { .. }
    ));
}

#[test]
fn add_project_draft_uses_project_loader_and_requires_confirmation_values() {
    let project = ProjectCandidateForTest::mod_project("Configurable Mod").into_candidate();

    let draft = AddProjectDraft::from_candidate(project, Some("cleanroom"));

    assert_eq!(draft.manifest, ManifestList::Common);
    assert_eq!(draft.loader.as_deref(), Some("cleanroom"));
    assert!(draft.auto_update);
    assert_eq!(draft.file_id, None);
    assert!(!draft.allows_file_version_selection());
    assert!(draft.required);
}

#[test]
fn add_project_draft_can_lock_file_id_before_entry_is_inserted() {
    let project = ProjectCandidateForTest::mod_project("Locked Mod").into_candidate();
    let mut draft = AddProjectDraft::from_candidate(project, Some("Forge"));
    draft.auto_update = false;
    draft.file_id = Some(5678);
    draft.manifest = ManifestList::Client;
    draft.required = false;
    assert!(draft.allows_file_version_selection());

    let entry = draft.into_entry();

    assert_eq!(entry.manifest, ManifestList::Client);
    assert_eq!(entry.loader.as_deref(), Some("forge"));
    assert_eq!(entry.file_id, Some(5678));
    assert!(!entry.auto_update);
    assert!(!entry.required);
    entry.validate().unwrap();
}

#[test]
fn add_project_draft_selecting_a_file_version_disables_auto_update() {
    let project = ProjectCandidateForTest::mod_project("Versioned Mod").into_candidate();
    let mut draft = AddProjectDraft::from_candidate(project, Some("forge"));

    draft.select_file_version(9876);

    assert!(!draft.auto_update);
    assert_eq!(draft.file_id, Some(9876));
    assert!(draft.allows_file_version_selection());
    draft.into_entry().validate().unwrap();
}

#[test]
fn parses_project_search_pid_urls_names_and_slugs() {
    assert_eq!(
        parse_project_search_input("12345").unwrap(),
        Some(ProjectSearchInput::ProjectId(12345))
    );
    assert_eq!(
        parse_project_search_input("https://www.curseforge.com/projects/67890").unwrap(),
        Some(ProjectSearchInput::ProjectId(67890))
    );
    assert_eq!(
        parse_project_search_input("https://www.curseforge.com/minecraft/mc-mods/sodium").unwrap(),
        Some(ProjectSearchInput::Keywords("sodium".to_string()))
    );
    assert_eq!(
        parse_project_search_input("Rubidium Extra").unwrap(),
        Some(ProjectSearchInput::Keywords("Rubidium Extra".to_string()))
    );
    assert_eq!(parse_project_search_input("   ").unwrap(), None);
}

#[test]
fn searches_supported_project_candidates_and_filters_unsupported_types() {
    let results = search_project_candidates(
        &SearchProjectsApi,
        &ProjectSearchInput::Keywords("example".to_string()),
    )
    .unwrap();

    assert_eq!(results.len(), 2);
    assert_eq!(results[0].name, "Example Mod");
    assert_eq!(results[0].authors, vec!["ExampleDev"]);
    assert_eq!(results[1].dependency_type, GuiDependencyType::ResourcePack);
}

#[test]
fn validates_save_structure_without_network_calls() {
    let mut entry = valid_gui_entry();
    entry.slug = None;
    entry.project_id = None;
    assert!(matches!(
        entry.validate().unwrap_err(),
        ModListValidationError::MissingProjectLocator { .. }
    ));

    entry.slug = Some("valid-slug".to_string());
    entry.project_id = Some(42);
    entry.manifest = ManifestList::Resourcepacks;
    entry.dependency_type = GuiDependencyType::Mod;
    assert!(matches!(
        entry.validate().unwrap_err(),
        ModListValidationError::InvalidResourcepackManifestType { .. }
    ));

    entry.manifest = ManifestList::Common;
    entry.dependency_type = GuiDependencyType::ResourcePack;
    assert!(matches!(
        entry.validate().unwrap_err(),
        ModListValidationError::InvalidModManifestType { .. }
    ));
}

#[test]
fn filters_entries_by_name_slug_project_id_or_file_id_while_preserving_groups() {
    let temp = tempfile::tempdir().unwrap();
    write_mod_list_project(temp.path());
    let list = GuiDependencyList::load(temp.path()).unwrap();

    let by_name = list.filtered_by_search("client");
    assert_eq!(
        by_name
            .mods()
            .iter()
            .map(|entry| entry.name.as_str())
            .collect::<Vec<_>>(),
        ["Client Mod"]
    );
    assert!(by_name.resource_packs().is_empty());
    assert!(by_name.shader_packs().is_empty());

    let by_slug = list.filtered_by_search("project-50");
    assert!(by_slug.mods().is_empty());
    assert!(by_slug.resource_packs().is_empty());
    assert_eq!(
        by_slug
            .shader_packs()
            .iter()
            .map(|entry| entry.name.as_str())
            .collect::<Vec<_>>(),
        ["Shader Pack"]
    );

    let by_project_id = list.filtered_by_search("40");
    assert_eq!(
        by_project_id
            .resource_packs()
            .iter()
            .map(|entry| entry.name.as_str())
            .collect::<Vec<_>>(),
        ["Resource Pack"]
    );

    let by_file_id = list.filtered_by_search("1030");
    assert_eq!(
        by_file_id
            .mods()
            .iter()
            .map(|entry| entry.name.as_str())
            .collect::<Vec<_>>(),
        ["Server Mod"]
    );

    assert_eq!(list.filtered_by_search("   "), list);
}

#[test]
fn file_candidate_query_omits_loader_filter_for_null_loader_and_passes_explicit_loader() {
    let api = RecordingApi::default();
    let query = QueryFileCandidates {
        project: ProjectLookup::ProjectId(10),
        minecraft_version: "1.20.1".to_string(),
        loader: None,
    };

    query_file_candidates(&api, &query).unwrap();
    assert_eq!(
        api.calls.borrow().as_slice(),
        ["project:10", "files:10:1.20.1:"]
    );

    let query = QueryFileCandidates {
        project: ProjectLookup::Slug("common-mod".to_string()),
        minecraft_version: "1.20.1".to_string(),
        loader: Some("forge".to_string()),
    };

    query_file_candidates(&api, &query).unwrap();
    assert_eq!(
        api.calls.borrow().as_slice(),
        [
            "project:10",
            "files:10:1.20.1:",
            "slug:common-mod",
            "files:10:1.20.1:forge"
        ]
    );
}

#[test]
fn file_candidate_query_cache_reuses_results_for_same_lookup_version_and_loader() {
    let api = RecordingApi::default();
    let cache = QueryFileCandidateCache::default();
    let query = QueryFileCandidates {
        project: ProjectLookup::ProjectId(10),
        minecraft_version: "1.20.1".to_string(),
        loader: Some("Forge".to_string()),
    };

    let first = query_file_candidates_cached(&api, &cache, &query).unwrap();
    let second = query_file_candidates_cached(&api, &cache, &query).unwrap();

    assert_eq!(first, second);
    assert_eq!(
        api.calls.borrow().as_slice(),
        ["project:10", "files:10:1.20.1:forge"]
    );
}

#[test]
fn file_candidate_query_cache_coalesces_concurrent_requests_for_same_key() {
    let api = Arc::new(BlockingRecordingApi::new());
    let cache = Arc::new(QueryFileCandidateCache::default());
    let query = QueryFileCandidates {
        project: ProjectLookup::Slug("common-mod".to_string()),
        minecraft_version: "1.20.1".to_string(),
        loader: None,
    };
    let ready = Arc::new(Barrier::new(2));
    let handles = (0..2)
        .map(|_| {
            let api = Arc::clone(&api);
            let cache = Arc::clone(&cache);
            let query = query.clone();
            let ready = Arc::clone(&ready);
            std::thread::spawn(move || {
                ready.wait();
                query_file_candidates_cached(api.as_ref(), cache.as_ref(), &query).unwrap()
            })
        })
        .collect::<Vec<_>>();

    let results = handles
        .into_iter()
        .map(|handle| handle.join().unwrap())
        .collect::<Vec<_>>();

    assert_eq!(results[0], results[1]);
    assert_eq!(
        api.calls.lock().unwrap().as_slice(),
        ["slug:common-mod", "files:10:1.20.1:"]
    );
}

#[test]
fn loader_options_include_null_config_loader_and_candidate_file_loaders() {
    let mut entry = valid_gui_entry();
    entry.loader = None;
    let candidates = vec![
        support::cf_file(101, 10, "forge.jar", &["1.20.1", "Forge"]),
        support::cf_file(102, 10, "fabric.jar", &["1.20.1", "Fabric"]),
    ];

    let options = entry.loader_options(Some("cleanroom"), &candidates);

    assert_eq!(
        options,
        vec![
            None,
            Some("cleanroom".to_string()),
            Some("fabric".to_string()),
            Some("forge".to_string())
        ]
    );
}

#[test]
fn mod_list_config_uses_unified_key_resolver_without_gui_key_fields() {
    let temp = tempfile::tempdir().unwrap();
    let config = support::sample_config();
    std::fs::write(
        temp.path().join("config.toml"),
        toml::to_string_pretty(&config).unwrap(),
    )
    .unwrap();

    let guard = EnvGuard::set_key_environment(" env-key ");
    let loaded = load_project_config_for_mod_list(temp.path()).unwrap();

    assert_eq!(
        loaded,
        ModListProjectConfig {
            minecraft_version: "1.12.2".to_string(),
            loader: "forge".to_string(),
            api_key: "env-key".to_string(),
            proxy: None,
        }
    );
    drop(guard);
}

#[test]
fn mod_list_config_allows_empty_legacy_curseforge_section_with_env_key() {
    let temp = tempfile::tempdir().unwrap();
    let config = format!(
        "{}\n[curseforge]\napi_key = \"\"\n",
        toml::to_string_pretty(&support::sample_config()).unwrap()
    );
    std::fs::write(temp.path().join("config.toml"), config).unwrap();

    let guard = EnvGuard::set_key_environment(" env-key ");
    let loaded = load_project_config_for_mod_list(temp.path()).unwrap();

    assert_eq!(loaded.api_key, "env-key");
    drop(guard);
}

fn write_mod_list_project(project: &Path) {
    std::fs::write(
        project.join("config.toml"),
        toml::to_string_pretty(&support::sample_config()).unwrap(),
    )
    .unwrap();
    support::write_entries(
        project.join("common.json"),
        &[named_entry(
            10,
            1010,
            DependencySide::Common,
            "Common Mod",
            DependencyType::Mod,
        )],
    );
    support::write_entries(
        project.join("client.json"),
        &[named_entry(
            20,
            1020,
            DependencySide::Client,
            "Client Mod",
            DependencyType::Mod,
        )],
    );
    support::write_entries(
        project.join("server.json"),
        &[named_entry(
            30,
            1030,
            DependencySide::Server,
            "Server Mod",
            DependencyType::Mod,
        )],
    );
    support::write_entries(
        project.join("resourcepacks.json"),
        &[
            named_entry(
                40,
                1040,
                DependencySide::Resourcepack,
                "Resource Pack",
                DependencyType::ResourcePack,
            ),
            named_entry(
                50,
                1050,
                DependencySide::Resourcepack,
                "Shader Pack",
                DependencyType::ShaderPack,
            ),
        ],
    );
}

fn read_json(path: impl AsRef<Path>) -> serde_json::Value {
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

fn valid_gui_entry() -> GuiModEntry {
    GuiModEntry {
        manifest: ManifestList::Common,
        name: "Common Mod".to_string(),
        dependency_type: GuiDependencyType::Mod,
        loader: Some("forge".to_string()),
        project_id: Some(10),
        file_id: Some(1010),
        auto_update: false,
        slug: Some("common-mod".to_string()),
        required: true,
    }
}

struct ProjectCandidateForTest {
    name: String,
}

struct SearchProjectsApi;

impl CurseForgeApi for SearchProjectsApi {
    fn get_project(&self, project_id: u64) -> Result<CfProject, ResolveError> {
        Err(ResolveError::Api(format!(
            "unexpected project lookup {project_id}"
        )))
    }

    fn get_file(&self, project_id: u64, file_id: u64) -> Result<CfFile, ResolveError> {
        Err(ResolveError::Api(format!(
            "unexpected file lookup {project_id}/{file_id}"
        )))
    }

    fn find_project_by_slug(&self, slug: &str) -> Result<CfProject, ResolveError> {
        Err(ResolveError::Api(format!("unexpected slug lookup {slug}")))
    }

    fn search_projects(&self, query: &str) -> Result<Vec<CfProject>, ResolveError> {
        assert_eq!(query, "example");
        Ok(vec![
            CfProject::new_for_test(101, "Example Mod", "example-mod", 6, vec!["ExampleDev"]),
            CfProject::new_for_test(
                102,
                "Example Resource Pack",
                "example-resource-pack",
                12,
                vec!["PackDev"],
            ),
            CfProject::new_for_test(103, "Unsupported", "unsupported", 999, vec!["OtherDev"]),
        ])
    }

    fn list_files(
        &self,
        project_id: u64,
        _minecraft_version: &str,
        _loader: &str,
    ) -> Result<Vec<CfFile>, ResolveError> {
        Err(ResolveError::Api(format!(
            "unexpected list files lookup for {project_id}"
        )))
    }
}

impl ProjectCandidateForTest {
    fn mod_project(name: &str) -> Self {
        Self {
            name: name.to_string(),
        }
    }

    fn into_candidate(self) -> mcmpu::gui::mod_list::ProjectCandidate {
        mcmpu::gui::mod_list::ProjectCandidate {
            project_id: 4242,
            slug: "configurable-mod".to_string(),
            name: self.name,
            authors: vec!["Test Author".to_string()],
            dependency_type: GuiDependencyType::Mod,
        }
    }
}

fn named_entry(
    project_id: u64,
    file_id: u64,
    side: DependencySide,
    name: &str,
    dependency_type: DependencyType,
) -> mcmpu::dependency::DependencyEntry {
    mcmpu::dependency::DependencyEntry {
        name: name.to_string(),
        dependency_type,
        ..support::entry(project_id, file_id, side)
    }
}

#[derive(Default)]
struct RecordingApi {
    calls: RefCell<Vec<String>>,
}

impl CurseForgeApi for RecordingApi {
    fn get_project(&self, project_id: u64) -> Result<CfProject, ResolveError> {
        self.calls
            .borrow_mut()
            .push(format!("project:{project_id}"));
        support::FixtureApi.get_project(project_id)
    }

    fn get_file(&self, project_id: u64, file_id: u64) -> Result<CfFile, ResolveError> {
        support::FixtureApi.get_file(project_id, file_id)
    }

    fn find_project_by_slug(&self, slug: &str) -> Result<CfProject, ResolveError> {
        self.calls.borrow_mut().push(format!("slug:{slug}"));
        support::FixtureApi.find_project_by_slug(slug)
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
        support::FixtureApi.list_files(project_id, minecraft_version, loader)
    }
}

struct BlockingRecordingApi {
    calls: Mutex<Vec<String>>,
}

impl BlockingRecordingApi {
    fn new() -> Self {
        Self {
            calls: Mutex::new(Vec::new()),
        }
    }
}

impl CurseForgeApi for BlockingRecordingApi {
    fn get_project(&self, project_id: u64) -> Result<CfProject, ResolveError> {
        self.calls
            .lock()
            .unwrap()
            .push(format!("project:{project_id}"));
        std::thread::sleep(std::time::Duration::from_millis(50));
        support::FixtureApi.get_project(project_id)
    }

    fn get_file(&self, project_id: u64, file_id: u64) -> Result<CfFile, ResolveError> {
        support::FixtureApi.get_file(project_id, file_id)
    }

    fn find_project_by_slug(&self, slug: &str) -> Result<CfProject, ResolveError> {
        self.calls.lock().unwrap().push(format!("slug:{slug}"));
        std::thread::sleep(std::time::Duration::from_millis(50));
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
            .push(format!("files:{project_id}:{minecraft_version}:{loader}"));
        std::thread::sleep(std::time::Duration::from_millis(50));
        support::FixtureApi.list_files(project_id, minecraft_version, loader)
    }
}

struct EnvGuard {
    _lock: MutexGuard<'static, ()>,
    values: Vec<(&'static str, Option<std::ffi::OsString>)>,
}

impl EnvGuard {
    fn set_key_environment(value: &str) -> Self {
        static ENV_LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        let lock = ENV_LOCK
            .get_or_init(|| Mutex::new(()))
            .lock()
            .expect("env test mutex poisoned");
        const KEYS: &[&str] = &[
            "MCMPU_DISABLE_PERSISTENT_ENV",
            "MCMODPACKUTIL_CURSEFORGE_API_KEY",
            "CURSEFORGE_API_KEY",
            "CF_API_KEY",
        ];
        let values = KEYS
            .iter()
            .map(|key| (*key, std::env::var_os(key)))
            .collect::<Vec<_>>();
        unsafe {
            std::env::set_var("MCMPU_DISABLE_PERSISTENT_ENV", "1");
            std::env::remove_var("MCMODPACKUTIL_CURSEFORGE_API_KEY");
            std::env::set_var("CURSEFORGE_API_KEY", value);
            std::env::remove_var("CF_API_KEY");
        }
        Self {
            _lock: lock,
            values,
        }
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        unsafe {
            for (key, value) in &self.values {
                if let Some(value) = value {
                    std::env::set_var(key, value);
                } else {
                    std::env::remove_var(key);
                }
            }
        }
    }
}
