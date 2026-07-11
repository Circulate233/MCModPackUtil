use serde_json::{Value, json};
use thiserror::Error;

use crate::config::Config;
use crate::dependency::DependencyEntry;

#[derive(Debug, Error)]
pub enum GeneratedManifestError {
    #[error("dependency '{name}' is missing project_id")]
    MissingProjectId { name: String },
    #[error("dependency '{name}' is missing file_id")]
    MissingFileId { name: String },
}

pub fn generate_modlist_html(entries: &[DependencyEntry]) -> String {
    let mut html = String::from("<ul>");
    for entry in entries {
        let project_id = entry.project_id.unwrap_or_default();
        let name = html_escape::encode_text(&entry.name);
        let authors_joined = entry.authors.join(", ");
        let authors = html_escape::encode_text(&authors_joined);
        html.push_str(&format!(
            "<li><a href=\"https://www.curseforge.com/projects/{project_id}\">{name} (by {authors})</a></li>"
        ));
    }
    html.push_str("</ul>");
    html
}

pub fn generate_pack_manifest(
    config: &Config,
    entries: &[DependencyEntry],
    overrides: &str,
) -> Result<Value, GeneratedManifestError> {
    let files = entries
        .iter()
        .map(|entry| {
            let project_id =
                entry
                    .project_id
                    .ok_or_else(|| GeneratedManifestError::MissingProjectId {
                        name: entry.name.clone(),
                    })?;
            let file_id = entry
                .file_id
                .ok_or_else(|| GeneratedManifestError::MissingFileId {
                    name: entry.name.clone(),
                })?;
            Ok(json!({
                "projectID": project_id,
                "fileID": file_id,
                "required": entry.required
            }))
        })
        .collect::<Result<Vec<_>, _>>()?;

    Ok(json!({
        "manifestType": "minecraftModpack",
        "manifestVersion": 1,
        "name": config.pack.name,
        "version": config.pack.version,
        "author": config.pack.authors.join(","),
        "overrides": overrides,
        "minecraft": {
            "version": config.pack.minecraft_version,
            "modLoaders": [{
                "id": format!("{}-{}", config.pack.loader.manifest_name(), config.pack.loader_version),
                "primary": true
            }]
        },
        "files": files
    }))
}
