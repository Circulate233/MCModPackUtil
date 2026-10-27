pub mod build_version;
pub mod cli;
pub mod config;
pub mod curseforge;
pub mod dependency;
pub mod gui;
pub mod i18n;
pub mod manifest;
pub mod net;
mod path_safety;
pub mod project;
pub mod server;
pub mod zip;

/// The version selected by `build.rs` for this binary.
pub const BUILD_VERSION: &str = env!("MCMPU_BUILD_VERSION");
