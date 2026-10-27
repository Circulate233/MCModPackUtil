use std::path::{Component, Path};

use thiserror::Error;

const MAX_PORTABLE_COMPONENT_UTF16: usize = 240;

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub(crate) enum PortablePathError {
    #[error("must not be empty")]
    Empty,
    #[error("must be a single portable path component")]
    NotSingleComponent,
    #[error("contains a character forbidden by Windows file names")]
    ForbiddenCharacter,
    #[error("must not end with a space or dot")]
    TrailingSpaceOrDot,
    #[error("uses reserved Windows device name '{name}'")]
    WindowsDevice { name: String },
    #[error("is longer than {MAX_PORTABLE_COMPONENT_UTF16} UTF-16 code units")]
    TooLong,
}

pub(crate) fn validate_portable_component(value: &str) -> Result<(), PortablePathError> {
    if value.is_empty() {
        return Err(PortablePathError::Empty);
    }
    if value == "."
        || value == ".."
        || value.contains(['/', '\\', '\0'])
        || has_windows_drive_prefix(value)
        || Path::new(value).is_absolute()
    {
        return Err(PortablePathError::NotSingleComponent);
    }
    if value.contains([':', '<', '>', '"', '|', '?', '*'])
        || value.chars().any(|character| character <= '\u{1f}')
    {
        return Err(PortablePathError::ForbiddenCharacter);
    }
    if value.trim_end_matches([' ', '.']) != value {
        return Err(PortablePathError::TrailingSpaceOrDot);
    }
    if value.encode_utf16().count() > MAX_PORTABLE_COMPONENT_UTF16 {
        return Err(PortablePathError::TooLong);
    }
    if is_windows_device_name(value) {
        return Err(PortablePathError::WindowsDevice {
            name: value.to_string(),
        });
    }
    Ok(())
}

pub(crate) fn validate_portable_relative_path(value: &str) -> Result<(), PortablePathError> {
    if value.is_empty()
        || value.starts_with('/')
        || value.contains(['\\', '\0'])
        || has_windows_drive_prefix(value)
        || Path::new(value).is_absolute()
        || value
            .split('/')
            .any(|component| component.is_empty() || component == "." || component == "..")
        || Path::new(value)
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(PortablePathError::NotSingleComponent);
    }
    for component in value.split('/') {
        validate_portable_component(component)?;
    }
    Ok(())
}

pub(crate) fn portable_path_identity(value: &str) -> Result<String, PortablePathError> {
    validate_portable_relative_path(value)?;
    Ok(value.to_lowercase())
}

pub(crate) fn validate_project_relative_directory(value: &str) -> Result<(), PortablePathError> {
    if value.is_empty()
        || value.contains(['\\', '\0', ':'])
        || value.starts_with('/')
        || has_windows_drive_prefix(value)
        || Path::new(value).is_absolute()
        || Path::new(value).components().any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
    {
        return Err(PortablePathError::NotSingleComponent);
    }
    for component in value.split('/') {
        if component == "." {
            continue;
        }
        validate_portable_component(component)?;
    }
    Ok(())
}

fn is_windows_device_name(value: &str) -> bool {
    let stem = value.split('.').next().unwrap_or_default();
    let upper = stem.to_uppercase();
    if matches!(
        upper.as_str(),
        "CON" | "PRN" | "AUX" | "NUL" | "CLOCK$" | "CONIN$" | "CONOUT$"
    ) {
        return true;
    }
    ["COM", "LPT"].iter().any(|prefix| {
        upper
            .strip_prefix(prefix)
            .is_some_and(is_reserved_device_number)
    })
}

fn is_reserved_device_number(value: &str) -> bool {
    matches!(
        value,
        "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³"
    )
}

fn has_windows_drive_prefix(value: &str) -> bool {
    let bytes = value.as_bytes();
    bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':'
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn portable_identity_rejects_windows_aliases_and_normalizes_case() {
        for value in [
            "CON",
            "con.txt",
            "COM1.jar",
            "LPT¹.cfg",
            "name:stream",
            "x. ",
        ] {
            assert!(
                validate_portable_component(value).is_err(),
                "accepted {value}"
            );
        }
        assert_eq!(
            portable_path_identity("Config/Ä.JSON").unwrap(),
            "config/ä.json"
        );
    }

    #[test]
    fn configured_output_must_remain_project_relative() {
        for value in ["../dist", "/tmp/dist", r"C:\dist", "safe/../../escape"] {
            assert!(
                validate_project_relative_directory(value).is_err(),
                "accepted {value}"
            );
        }
        validate_project_relative_directory("dist/releases").unwrap();
        validate_project_relative_directory(".").unwrap();
    }
}
