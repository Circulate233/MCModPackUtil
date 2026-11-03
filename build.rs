#[path = "src/build_version.rs"]
mod build_version;

const CURSEFORGE_API_KEY_ENV: &[&str] = &[
    "MCMODPACKUTIL_CURSEFORGE_API_KEY",
    "CURSEFORGE_API_KEY",
    "CF_API_KEY",
];

fn main() {
    for name in CURSEFORGE_API_KEY_ENV {
        println!("cargo:rerun-if-env-changed={name}");
    }
    println!("cargo:rerun-if-env-changed=MCMODPACKUTIL_BUILD_VERSION");

    let repository = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let release_override = std::env::var_os("MCMODPACKUTIL_BUILD_VERSION");
    if release_override.is_none() {
        build_version::emit_git_rerun_dependencies(repository)
            .unwrap_or_else(|error| panic!("cannot declare Git version dependencies: {error}"));
    }

    let version = build_version::resolve_build_version(repository, release_override.as_deref())
        .unwrap_or_else(|error| panic!("cannot resolve build version: {error}"));
    println!("cargo:rustc-env=MCMPU_BUILD_VERSION={}", version.value);

    let api_key = CURSEFORGE_API_KEY_ENV
        .iter()
        .filter_map(|name| std::env::var(name).ok())
        .map(|value| value.trim().to_string())
        .find(|value| !value.is_empty())
        .unwrap_or_default();
    assert!(
        !api_key.chars().any(char::is_control),
        "CurseForge API key must not contain control characters"
    );
    println!("cargo:rustc-env=MCMPU_BUILTIN_CURSEFORGE_API_KEY={api_key}");
}
