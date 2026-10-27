#![cfg(feature = "gui")]

use mcmpu::gui::loader_versions::{
    GuiNetworkEngineCache, LoaderVersionCandidateState, cleanroom_versions_for_minecraft,
    forge_versions_for_minecraft, neoforge_versions_for_minecraft, parse_maven_versions,
};

#[test]
fn candidate_state_starts_idle_and_requires_explicit_user_request() {
    let mut state = LoaderVersionCandidateState::default();

    assert!(state.can_request());
    assert!(state.begin_request());
    assert_eq!(state, LoaderVersionCandidateState::Loading);
    assert!(!state.can_request());
    assert!(!state.begin_request());
}

#[test]
fn candidate_state_retries_after_failure_and_invalidates_after_query_change() {
    let mut state = LoaderVersionCandidateState::default();
    assert!(state.begin_request());

    state.finish_request(Err("network unavailable".to_string()));
    assert_eq!(state.error(), Some("network unavailable"));
    assert!(state.can_request());

    assert!(state.begin_request());
    state.finish_request(Ok(()));
    assert_eq!(state, LoaderVersionCandidateState::Ready);
    assert!(!state.can_request());

    state.invalidate();
    assert_eq!(state, LoaderVersionCandidateState::Idle);
    assert!(state.can_request());
}

#[test]
fn maven_metadata_is_parsed_and_forge_versions_are_filtered_by_minecraft() {
    let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<metadata>
  <versioning>
    <versions>
      <version>1.12.2-14.23.5.2859</version>
      <version>1.20.1-47.3.22</version>
      <version>1.12.2-14.23.5.2860</version>
    </versions>
  </versioning>
</metadata>"#;

    let versions = parse_maven_versions(xml).unwrap();
    let filtered = forge_versions_for_minecraft("1.12.2", versions);

    assert_eq!(filtered, vec!["14.23.5.2860", "14.23.5.2859"]);
}

#[test]
fn cleanroom_versions_are_limited_to_1_12_2_and_exclude_build_artifacts() {
    let versions = vec![
        "0.6.2-alpha".to_string(),
        "0.6.3-build.4".to_string(),
        "0.6.1-alpha".to_string(),
    ];

    assert_eq!(
        cleanroom_versions_for_minecraft("1.12.2", versions.clone()),
        vec!["0.6.2-alpha", "0.6.1-alpha"]
    );
    assert!(cleanroom_versions_for_minecraft("1.20.1", versions).is_empty());
}

#[test]
fn neoforge_versions_follow_the_minecraft_minor_and_patch_pair() {
    let versions = vec![
        "20.2.93-beta".to_string(),
        "20.4.237".to_string(),
        "21.1.241".to_string(),
        "21.1.200".to_string(),
    ];

    assert_eq!(
        neoforge_versions_for_minecraft("1.21.1", versions),
        vec!["21.1.241", "21.1.200"]
    );
}

#[test]
fn metadata_network_cache_reuses_one_engine_for_equivalent_proxy_values() {
    let cache = GuiNetworkEngineCache::default();

    let first = cache
        .loader_version_client(Some("  http://127.0.0.1:7890  "))
        .unwrap();
    let second = cache.network_engine(Some("http://127.0.0.1:7890")).unwrap();

    let _cloned_handles = (first.clone(), second.clone());
    assert_eq!(cache.cached_engine_count().unwrap(), 1);
}

#[test]
fn metadata_network_cache_fails_fast_for_an_invalid_proxy() {
    let cache = GuiNetworkEngineCache::default();

    let Err(error) = cache.network_engine(Some("not a proxy url")) else {
        panic!("invalid proxy should fail before entering the cache");
    };

    assert!(error.to_string().contains("invalid proxy URL"));
    assert_eq!(cache.cached_engine_count().unwrap(), 0);
}

#[test]
fn metadata_network_cache_replaces_the_previous_proxy_engine() {
    let cache = GuiNetworkEngineCache::default();

    cache.network_engine(Some("http://127.0.0.1:7890")).unwrap();
    cache.network_engine(Some("http://127.0.0.1:7891")).unwrap();

    assert_eq!(cache.cached_engine_count().unwrap(), 1);
}
