#[path = "src/build_version.rs"]
mod build_version;

fn main() {
    println!("cargo:rerun-if-env-changed=MCMODPACKUTIL_CURSEFORGE_API_KEY");
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

    if std::env::var("PROFILE").as_deref() != Ok("release") {
        return;
    }

    if let Ok(api_key) = std::env::var("MCMODPACKUTIL_CURSEFORGE_API_KEY") {
        let trimmed = api_key.trim();
        if !trimmed.is_empty() {
            println!("cargo:rustc-env=MCMPU_BUILTIN_CURSEFORGE_API_KEY={trimmed}");
        }
    }
}
