use std::collections::BTreeMap;

use mcmpu::config::LoaderKind;
use mcmpu::project::launcher::{
    LaunchArtifact, LaunchEnvironment, LaunchSide, LoaderLaunchResolver, ServerLaunchMetadata,
    VersionJsonSource,
};

#[derive(Default)]
struct FixtureVersions {
    versions: BTreeMap<String, String>,
}

impl FixtureVersions {
    fn with(mut self, id: &str, json: &str) -> Self {
        self.versions.insert(id.to_string(), json.to_string());
        self
    }
}

impl VersionJsonSource for FixtureVersions {
    fn load_version_json(
        &self,
        version_id: &str,
    ) -> Result<String, mcmpu::project::launcher::LaunchResolveError> {
        self.versions.get(version_id).cloned().ok_or_else(|| {
            mcmpu::project::launcher::LaunchResolveError::VersionNotFound {
                version_id: version_id.to_string(),
            }
        })
    }
}

#[test]
fn resolves_inherited_loader_profile_into_launch_descriptor() {
    let source = FixtureVersions::default()
        .with("1.20.1", VANILLA_1_20_1)
        .with("fabric-loader", FABRIC_PROFILE);
    let resolver = LoaderLaunchResolver::new(source);
    let environment =
        LaunchEnvironment::new("windows", "x86_64", 64).with_feature("has_custom_resolution", true);

    let descriptor = resolver
        .resolve_client(LoaderKind::Fabric, "fabric-loader", &environment)
        .unwrap();

    assert_eq!(descriptor.side, LaunchSide::Client);
    assert_eq!(
        descriptor.main_class,
        "net.fabricmc.loader.impl.launch.knot.KnotClient"
    );
    assert_eq!(
        descriptor.inheritance_chain,
        vec!["1.20.1", "fabric-loader"]
    );
    assert_eq!(descriptor.java_version_major, Some(17));
    assert_eq!(descriptor.assets.as_deref(), Some("5"));
    assert_eq!(descriptor.asset_index.as_ref().unwrap().id, "5");
    assert!(
        descriptor
            .game_arguments
            .windows(2)
            .any(|args| args == ["--width", "${resolution_width}"])
    );
    assert!(
        descriptor
            .jvm_arguments
            .contains(&"-Dfabric=true".to_string())
    );
    assert!(
        descriptor
            .classpath
            .iter()
            .any(|path| path == "libraries/com/google/code/gson/gson/2.10/gson-2.10.jar")
    );
    assert!(descriptor.classpath.iter().any(
        |path| path == "libraries/net/fabricmc/fabric-loader/0.15.11/fabric-loader-0.15.11.jar"
    ));
    assert_eq!(
        descriptor.client_jar.as_ref().unwrap().path,
        "versions/1.20.1/1.20.1.jar"
    );
}

#[test]
fn loader_main_classes_are_consumed_from_profiles_without_loader_command_guesses() {
    for (loader, id, main_class) in [
        (
            LoaderKind::Forge,
            "forge",
            "cpw.mods.bootstraplauncher.BootstrapLauncher",
        ),
        (
            LoaderKind::NeoForge,
            "neoforge",
            "cpw.mods.bootstraplauncher.BootstrapLauncher",
        ),
        (
            LoaderKind::Cleanroom,
            "cleanroom",
            "top.outlands.foundation.boot.Foundation",
        ),
    ] {
        let profile = format!(
            r#"{{"id":"{id}","mainClass":"{main_class}","minecraftArguments":"--username ${{auth_player_name}}","libraries":[],"downloads":{{"client":{{"url":"https://example.invalid/client.jar","sha1":"abc","size":1}}}}}}"#
        );
        let resolver = LoaderLaunchResolver::new(FixtureVersions::default().with(id, &profile));

        let descriptor = resolver
            .resolve_client(loader, id, &LaunchEnvironment::new("linux", "x86_64", 64))
            .unwrap();

        assert_eq!(descriptor.main_class, main_class);
        assert_eq!(
            descriptor.game_arguments,
            vec!["--username", "${auth_player_name}"]
        );
    }
}

#[test]
fn child_legacy_minecraft_arguments_replace_the_parent_complete_command_line() {
    let parent = r#"{
      "id":"1.12.2",
      "mainClass":"net.minecraft.client.main.Main",
      "minecraftArguments":"--username ${auth_player_name} --gameDir ${game_directory}"
    }"#;
    let child = r#"{
      "id":"forge",
      "inheritsFrom":"1.12.2",
      "mainClass":"net.minecraft.launchwrapper.Launch",
      "minecraftArguments":"--username ${auth_player_name} --gameDir ${game_directory} --tweakClass net.minecraftforge.fml.common.launcher.FMLTweaker"
    }"#;
    let descriptor = LoaderLaunchResolver::new(
        FixtureVersions::default()
            .with("1.12.2", parent)
            .with("forge", child),
    )
    .resolve_client(
        LoaderKind::Forge,
        "forge",
        &LaunchEnvironment::new("windows", "x86_64", 64),
    )
    .unwrap();

    assert_eq!(
        descriptor.game_arguments,
        vec![
            "--username",
            "${auth_player_name}",
            "--gameDir",
            "${game_directory}",
            "--tweakClass",
            "net.minecraftforge.fml.common.launcher.FMLTweaker"
        ]
    );
}

#[test]
fn applies_library_rules_and_resolves_native_classifier_metadata() {
    let resolver =
        LoaderLaunchResolver::new(FixtureVersions::default().with("native", NATIVE_PROFILE));
    let environment = LaunchEnvironment::new("windows", "x86", 32);

    let descriptor = resolver
        .resolve_client(LoaderKind::Cleanroom, "native", &environment)
        .unwrap();

    assert!(
        descriptor
            .classpath
            .iter()
            .any(|path| path.ends_with("allowed-1.0.jar"))
    );
    assert!(
        !descriptor
            .classpath
            .iter()
            .any(|path| path.contains("linux-only"))
    );
    assert_eq!(descriptor.natives.len(), 1);
    assert_eq!(descriptor.natives[0].classifier, "natives-windows-32");
    assert_eq!(
        descriptor.natives[0].artifact.path,
        "libraries/org/example/native/1.0/native-1.0-natives-windows-32.jar"
    );
    assert_eq!(descriptor.natives[0].extract_excludes, vec!["META-INF/"]);
    assert!(
        descriptor
            .classpath
            .iter()
            .all(|path| !path.ends_with("native-1.0.jar"))
    );
}

#[test]
fn child_profile_replaces_parent_maven_library_version_in_classpath() {
    let parent = r#"
{
  "id":"1.12.2",
  "mainClass":"net.minecraft.client.main.Main",
  "libraries":[{"name":"example:conflict:1.0","url":"https://repo.example/"}]
}
"#;
    let child = r#"
{
  "id":"cleanroom",
  "inheritsFrom":"1.12.2",
  "mainClass":"top.outlands.foundation.boot.Foundation",
  "libraries":[{"name":"example:conflict:2.0","url":"https://repo.example/"}]
}
"#;
    let descriptor = LoaderLaunchResolver::new(
        FixtureVersions::default()
            .with("1.12.2", parent)
            .with("cleanroom", child),
    )
    .resolve_client(
        LoaderKind::Cleanroom,
        "cleanroom",
        &LaunchEnvironment::new("windows", "x86_64", 64),
    )
    .unwrap();

    assert_eq!(descriptor.libraries.len(), 1);
    assert_eq!(
        descriptor.libraries[0].path,
        "libraries/example/conflict/2.0/conflict-2.0.jar"
    );
    assert_eq!(
        descriptor.classpath,
        vec![descriptor.libraries[0].path.clone()]
    );
}

#[test]
fn normalizes_existing_library_prefix_without_duplicating_it() {
    let profile = r#"
{
  "id":"prefixed",
  "mainClass":"example.Main",
  "libraries":[{
    "name":"example:prefixed:1.0",
    "downloads":{"artifact":{"path":"libraries/example/prefixed/1.0/prefixed-1.0.jar","url":"https://repo.example/prefixed.jar"}}
  }]
}
"#;

    let descriptor =
        LoaderLaunchResolver::new(FixtureVersions::default().with("prefixed", profile))
            .resolve_client(
                LoaderKind::Forge,
                "prefixed",
                &LaunchEnvironment::new("windows", "x86_64", 64),
            )
            .unwrap();

    assert_eq!(
        descriptor.libraries[0].path,
        "libraries/example/prefixed/1.0/prefixed-1.0.jar"
    );
    assert_eq!(
        descriptor.classpath,
        vec![descriptor.libraries[0].path.clone()]
    );
}

#[test]
fn cleanroom_coordinate_uses_cleanroom_releases_when_profile_has_no_repository() {
    let profile = r#"
{
  "id":"cleanroom",
  "mainClass":"top.outlands.foundation.boot.Foundation",
  "libraries":[{"name":"com.cleanroommc:cleanroom:0.6.4-alpha"}]
}
"#;

    let descriptor =
        LoaderLaunchResolver::new(FixtureVersions::default().with("cleanroom", profile))
            .resolve_client(
                LoaderKind::Cleanroom,
                "cleanroom",
                &LaunchEnvironment::new("windows", "x86_64", 64),
            )
            .unwrap();

    assert_eq!(
        descriptor.libraries[0].path,
        "libraries/com/cleanroommc/cleanroom/0.6.4-alpha/cleanroom-0.6.4-alpha.jar"
    );
    assert_eq!(
        descriptor.libraries[0].url.as_deref(),
        Some(
            "https://repo.cleanroommc.com/releases/com/cleanroommc/cleanroom/0.6.4-alpha/cleanroom-0.6.4-alpha.jar"
        )
    );
}

#[test]
fn cleanroom_repository_rule_does_not_change_other_default_repository_cases() {
    let cleanroom_coordinate_profile = r#"
{
  "id":"profile",
  "mainClass":"example.Main",
  "libraries":[{"name":"com.cleanroommc:cleanroom:0.6.4-alpha"}]
}
"#;
    for loader in [LoaderKind::Forge, LoaderKind::Fabric, LoaderKind::NeoForge] {
        let descriptor = LoaderLaunchResolver::new(
            FixtureVersions::default().with("profile", cleanroom_coordinate_profile),
        )
        .resolve_client(
            loader,
            "profile",
            &LaunchEnvironment::new("windows", "x86_64", 64),
        )
        .unwrap();

        assert_eq!(
            descriptor.libraries[0].url.as_deref(),
            Some(
                "https://libraries.minecraft.net/com/cleanroommc/cleanroom/0.6.4-alpha/cleanroom-0.6.4-alpha.jar"
            )
        );
    }

    let other_coordinate_profile = r#"
{
  "id":"profile",
  "mainClass":"example.Main",
  "libraries":[{"name":"org.example:library:1.0"}]
}
"#;
    let descriptor = LoaderLaunchResolver::new(
        FixtureVersions::default().with("profile", other_coordinate_profile),
    )
    .resolve_client(
        LoaderKind::Cleanroom,
        "profile",
        &LaunchEnvironment::new("windows", "x86_64", 64),
    )
    .unwrap();

    assert_eq!(
        descriptor.libraries[0].url.as_deref(),
        Some("https://libraries.minecraft.net/org/example/library/1.0/library-1.0.jar")
    );
}

#[test]
fn child_profile_replacement_removes_parent_native_artifact() {
    let parent = r#"
{
  "id":"1.12.2",
  "mainClass":"net.minecraft.client.main.Main",
  "libraries":[{
    "name":"example:conflict:1.0",
    "url":"https://repo.example/",
    "natives":{"windows":"natives-windows"},
    "downloads":{"classifiers":{"natives-windows":{"url":"https://repo.example/old-native.jar","sha1":"old","size":1}}}
  }]
}
"#;
    let child = r#"
{
  "id":"cleanroom",
  "inheritsFrom":"1.12.2",
  "mainClass":"top.outlands.foundation.boot.Foundation",
  "libraries":[{"name":"example:conflict:2.0","url":"https://repo.example/"}]
}
"#;
    let descriptor = LoaderLaunchResolver::new(
        FixtureVersions::default()
            .with("1.12.2", parent)
            .with("cleanroom", child),
    )
    .resolve_client(
        LoaderKind::Cleanroom,
        "cleanroom",
        &LaunchEnvironment::new("windows", "x86_64", 64),
    )
    .unwrap();

    assert!(descriptor.natives.is_empty());
}

#[test]
fn explicit_server_metadata_produces_server_descriptor() {
    let resolver = LoaderLaunchResolver::new(FixtureVersions::default());
    let metadata = ServerLaunchMetadata {
        version_id: "installed-forge-server".to_string(),
        main_class: "cpw.mods.bootstraplauncher.BootstrapLauncher".to_string(),
        libraries: vec![LaunchArtifact {
            path: "libraries/cpw/mods/bootstraplauncher/1.1.2/bootstraplauncher-1.1.2.jar"
                .to_string(),
            url: None,
            sha1: Some("abc".to_string()),
            size: Some(10),
        }],
        jvm_arguments: vec![
            "@user_jvm_args.txt".to_string(),
            "@libraries/forge/win_args.txt".to_string(),
        ],
        game_arguments: vec!["nogui".to_string()],
        java_version_major: Some(17),
    };

    let descriptor = resolver
        .resolve_server(LoaderKind::Forge, metadata)
        .unwrap();

    assert_eq!(descriptor.side, LaunchSide::Server);
    assert_eq!(
        descriptor.classpath,
        vec!["libraries/cpw/mods/bootstraplauncher/1.1.2/bootstraplauncher-1.1.2.jar"]
    );
    assert!(descriptor.asset_index.is_none());
    assert!(descriptor.natives.is_empty());
}

#[test]
fn inheritance_cycles_fail_fast() {
    let resolver = LoaderLaunchResolver::new(
        FixtureVersions::default()
            .with("a", r#"{"id":"a","inheritsFrom":"b"}"#)
            .with("b", r#"{"id":"b","inheritsFrom":"a"}"#),
    );

    let error = resolver
        .resolve_client(
            LoaderKind::Fabric,
            "a",
            &LaunchEnvironment::new("linux", "x86_64", 64),
        )
        .unwrap_err();

    assert!(error.to_string().contains("inheritance cycle"));
}

const VANILLA_1_20_1: &str = r#"
{
  "id": "1.20.1",
  "mainClass": "net.minecraft.client.main.Main",
  "assets": "5",
  "assetIndex": {"id":"5","url":"https://example.invalid/assets.json","sha1":"asset-sha1","size":10,"totalSize":20},
  "javaVersion": {"component":"java-runtime-gamma","majorVersion":17},
  "downloads": {"client":{"url":"https://example.invalid/client.jar","sha1":"client-sha1","size":100}},
  "arguments": {
    "game": ["--username","${auth_player_name}",{"rules":[{"action":"allow","features":{"has_custom_resolution":true}}],"value":["--width","${resolution_width}"]}],
    "jvm": ["-Djava.library.path=${natives_directory}","-cp","${classpath}"]
  },
  "libraries": [{"name":"com.google.code.gson:gson:2.10","downloads":{"artifact":{"path":"com/google/code/gson/gson/2.10/gson-2.10.jar","url":"https://example.invalid/gson.jar","sha1":"gson-sha1","size":12}}}]
}
"#;

const FABRIC_PROFILE: &str = r#"
{
  "id":"fabric-loader",
  "inheritsFrom":"1.20.1",
  "mainClass":"net.fabricmc.loader.impl.launch.knot.KnotClient",
  "arguments":{"game":[],"jvm":["-Dfabric=true"]},
  "libraries":[{"name":"net.fabricmc:fabric-loader:0.15.11","url":"https://maven.fabricmc.net/"}]
}
"#;

const NATIVE_PROFILE: &str = r#"
{
  "id":"native",
  "mainClass":"top.outlands.foundation.boot.Foundation",
  "minecraftArguments":"--username ${auth_player_name} --quoted \"two words\"",
  "downloads":{"client":{"url":"https://example.invalid/client.jar","sha1":"client","size":1}},
  "libraries":[
    {"name":"org.example:allowed:1.0","url":"https://repo.example/","rules":[{"action":"allow"}]},
    {"name":"org.example:linux-only:1.0","url":"https://repo.example/","rules":[{"action":"allow","os":{"name":"linux"}}]},
    {"name":"org.example:native:1.0","url":"https://repo.example/","natives":{"windows":"natives-windows-${arch}"},"extract":{"exclude":["META-INF/"]},"downloads":{"classifiers":{"natives-windows-32":{"url":"https://repo.example/native.jar","sha1":"native","size":2}}}}
  ]
}
"#;
