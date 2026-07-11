fn main() {
    println!("cargo:rerun-if-env-changed=MCMODPACKUTIL_CURSEFORGE_API_KEY");
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
