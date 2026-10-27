use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::Command;

const DEFAULT_BASE_VERSION: &str = "0.0.0";

/// The version selected for one build, including its source and hash suffix.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedBuildVersion {
    /// The exact version string exposed by the binary.
    pub value: String,
}

/// Resolves a release override or the nearest reachable legal release tag.
///
/// An override is deliberately resolved before touching Git so release builds
/// from source archives do not need repository metadata. Ordinary builds must
/// have a readable `HEAD`; a missing repository is an error rather than an
/// implicit compatibility version.
pub fn resolve_build_version(
    repository: &Path,
    release_override: Option<&OsStr>,
) -> Result<ResolvedBuildVersion, String> {
    if let Some(override_value) = release_override {
        let value = override_value.to_str().ok_or_else(|| {
            "MCMODPACKUTIL_BUILD_VERSION must be valid UTF-8 and match X.Y.Z".to_string()
        })?;
        validate_release_version(value)?;
        return Ok(ResolvedBuildVersion {
            value: value.to_string(),
        });
    }

    GitLayout::discover(repository)?;
    let head = git(repository, ["rev-parse", "--verify", "HEAD"])?;
    let head = head.trim();
    if head.len() < 7 || !head.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(format!(
            "Git HEAD is not a full hexadecimal object id: '{head}'"
        ));
    }

    let valid_tags = git(
        repository,
        ["tag", "--merged", "HEAD", "--format=%(refname:strip=2)"],
    )?
    .lines()
    .map(str::trim)
    .filter(|tag| is_legal_release_tag(tag))
    .map(ToOwned::to_owned)
    .collect::<Vec<_>>();

    let base = if valid_tags.is_empty() {
        DEFAULT_BASE_VERSION.to_string()
    } else {
        nearest_release_tag(repository, &valid_tags)?
    };

    Ok(ResolvedBuildVersion {
        value: format!("{base}+{}", &head[..7]),
    })
}

/// Returns whether a string is a strict three-component decimal release version.
pub fn is_legal_release_version(value: &str) -> bool {
    let mut components = value.split('.');
    let valid = components.by_ref().all(is_decimal_component);
    valid && components.next().is_none() && value.split('.').count() == 3
}

/// Validates a strict `X.Y.Z` release override and returns a diagnostic on error.
pub fn validate_release_version(value: &str) -> Result<(), String> {
    if is_legal_release_version(value) {
        Ok(())
    } else {
        Err(format!(
            "MCMODPACKUTIL_BUILD_VERSION must match strict X.Y.Z (got '{value}')"
        ))
    }
}

/// Emits Cargo dependencies for Git metadata used by ordinary builds.
pub fn emit_git_rerun_dependencies(repository: &Path) -> Result<(), String> {
    let layout = GitLayout::discover(repository)?;
    for path in layout.rerun_paths(repository) {
        println!("cargo:rerun-if-changed={}", path.display());
    }
    Ok(())
}

fn is_legal_release_tag(tag: &str) -> bool {
    tag.strip_prefix('v').is_some_and(is_legal_release_version)
}

fn nearest_release_tag(repository: &Path, tags: &[String]) -> Result<String, String> {
    let mut args = vec![
        "describe".to_string(),
        "--tags".to_string(),
        "--abbrev=0".to_string(),
        format!("--candidates={}", tags.len()),
    ];
    for tag in tags {
        args.push("--match".to_string());
        args.push(tag.clone());
    }
    let args = args.iter().map(String::as_str).collect::<Vec<_>>();
    let selected = git(repository, args)?;
    let selected = selected.trim();
    if !tags.iter().any(|tag| tag == selected) {
        return Err(format!(
            "Git selected an unexpected release tag '{selected}'"
        ));
    }
    Ok(selected
        .strip_prefix('v')
        .expect("validated release tag has v prefix")
        .to_string())
}

fn is_decimal_component(component: &str) -> bool {
    !component.is_empty()
        && component.bytes().all(|byte| byte.is_ascii_digit())
        && (component == "0" || !component.starts_with('0'))
}

fn git<'a, I>(repository: &Path, args: I) -> Result<String, String>
where
    I: IntoIterator<Item = &'a str>,
{
    let args = args.into_iter().collect::<Vec<_>>();
    let output = Command::new("git")
        .arg("-C")
        .arg(repository)
        .args(&args)
        .output()
        .map_err(|source| format!("failed to execute Git for {:?}: {source}", args))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(format!(
            "Git {:?} failed with {}: {}",
            args,
            output.status,
            if stderr.is_empty() {
                "no diagnostic output"
            } else {
                &stderr
            }
        ));
    }
    String::from_utf8(output.stdout)
        .map_err(|source| format!("Git {:?} returned non-UTF-8 output: {source}", args))
}

#[derive(Debug)]
struct GitLayout {
    git_dir: PathBuf,
    common_dir: PathBuf,
}

impl GitLayout {
    fn discover(repository: &Path) -> Result<Self, String> {
        let git_dir = resolve_git_path(repository, &git(repository, ["rev-parse", "--git-dir"])?)?;
        let common_dir = resolve_git_path(
            repository,
            &git(repository, ["rev-parse", "--git-common-dir"])?,
        )?;
        Ok(Self {
            git_dir,
            common_dir,
        })
    }

    fn rerun_paths(&self, repository: &Path) -> [PathBuf; 9] {
        [
            repository.join(".git"),
            self.git_dir.join("HEAD"),
            self.git_dir.join("packed-refs"),
            self.common_dir.join("HEAD"),
            self.common_dir.join("packed-refs"),
            self.git_dir.join("refs").join("heads"),
            self.git_dir.join("refs").join("tags"),
            self.common_dir.join("refs").join("heads"),
            self.common_dir.join("refs").join("tags"),
        ]
    }
}

fn resolve_git_path(repository: &Path, raw: &str) -> Result<PathBuf, String> {
    let path = PathBuf::from(raw.trim());
    if path.as_os_str().is_empty() {
        return Err("Git returned an empty repository metadata path".to_string());
    }
    Ok(if path.is_absolute() {
        path
    } else {
        repository.join(path)
    })
}

#[cfg(test)]
mod tests {
    use super::{is_legal_release_version, validate_release_version};

    #[test]
    fn strict_release_version_rules_are_numeric_and_zero_safe() {
        for value in ["0.0.0", "1.20.300", "10.2.3"] {
            assert!(is_legal_release_version(value));
            assert_eq!(validate_release_version(value), Ok(()));
        }
        for value in ["", "1.2", "1.2.3.4", "01.2.3", "1.02.3", "v1.2.3", "1.a.3"] {
            assert!(!is_legal_release_version(value));
            assert!(validate_release_version(value).is_err());
        }
    }
}
