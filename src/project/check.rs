use std::path::Path;

use crate::curseforge::client::CurseForgeClient;
use crate::curseforge::resolver::{ApiKeyResolver, CurseForgeApi};
use crate::project::build::{
    BuildError, BuildProgress, BuildValidationReport, validate_project_with_api,
    validate_project_with_api_and_progress,
};

pub fn check_project(project_dir: impl AsRef<Path>) -> Result<BuildValidationReport, BuildError> {
    let project_dir = project_dir.as_ref();
    let config = crate::config::Config::from_path(project_dir.join("config.toml"))?;
    let env_api_key = ApiKeyResolver::env_key();
    let builtin_api_key = ApiKeyResolver::builtin_key();
    let api_key = ApiKeyResolver::resolve(
        None,
        env_api_key.as_deref(),
        builtin_api_key.as_deref().unwrap_or(""),
    )?;
    let config_proxy = proxy_from_config(&config)?;
    let api = CurseForgeClient::with_proxy(api_key, config_proxy.as_deref())?;
    validate_project_with_api(project_dir, &api)
}

pub fn check_project_with_api_key(
    project_dir: impl AsRef<Path>,
    api_key: String,
    proxy: Option<&str>,
) -> Result<BuildValidationReport, BuildError> {
    check_project_with_api_key_and_progress(project_dir, api_key, proxy, &mut |_| {})
}

pub fn check_project_with_api_key_and_progress<F>(
    project_dir: impl AsRef<Path>,
    api_key: String,
    proxy: Option<&str>,
    progress: &mut F,
) -> Result<BuildValidationReport, BuildError>
where
    F: FnMut(&BuildProgress) + Send,
{
    let project_dir = project_dir.as_ref();
    let config = crate::config::Config::from_path(project_dir.join("config.toml"))?;
    let config_proxy = proxy_from_config(&config)?;
    let api = CurseForgeClient::with_proxy(api_key, proxy.or(config_proxy.as_deref()))?;
    validate_project_with_api_and_progress(project_dir, &api, progress)
}

fn proxy_from_config(config: &crate::config::Config) -> Result<Option<String>, BuildError> {
    config.network.proxy_url().map_err(Into::into)
}

pub fn check_project_with_api<A: CurseForgeApi + Sync>(
    project_dir: impl AsRef<Path>,
    api: &A,
) -> Result<BuildValidationReport, BuildError> {
    validate_project_with_api(project_dir, api)
}

pub fn check_project_with_api_and_progress<A, F>(
    project_dir: impl AsRef<Path>,
    api: &A,
    progress: &mut F,
) -> Result<BuildValidationReport, BuildError>
where
    A: CurseForgeApi + Sync,
    F: FnMut(&BuildProgress) + Send,
{
    validate_project_with_api_and_progress(project_dir, api, progress)
}
