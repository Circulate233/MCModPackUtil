use crate::config::JavaMemoryConfig;
use thiserror::Error;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum JvmArgumentError {
    #[error("JVM argument '{argument}' must not override configured -Xms or -Xmx")]
    MemoryOverride { argument: String },
}

/// Splits configured JVM argument entries using Rust's standard Unicode whitespace rules.
///
/// The configuration format deliberately does not support shell quoting or escaping. Every
/// whitespace-delimited token becomes one process argument.
pub fn split_jvm_argument_entries(entries: &[String]) -> Vec<String> {
    entries
        .iter()
        .flat_map(|entry| entry.split_whitespace())
        .map(str::to_string)
        .collect()
}

/// Builds memory arguments followed by normalized extra JVM arguments.
pub fn java_memory_arguments(java: &JavaMemoryConfig) -> Result<Vec<String>, JvmArgumentError> {
    let mut arguments = vec![
        format!("-Xms{}", java.min_memory),
        format!("-Xmx{}", java.max_memory),
    ];
    append_extra_arguments(&mut arguments, &java.extra_args)?;
    Ok(arguments)
}

pub fn append_extra_arguments(
    arguments: &mut Vec<String>,
    entries: &[String],
) -> Result<(), JvmArgumentError> {
    for argument in split_jvm_argument_entries(entries) {
        if argument.starts_with("-Xms") || argument.starts_with("-Xmx") {
            return Err(JvmArgumentError::MemoryOverride { argument });
        }
        arguments.push(argument);
    }
    Ok(())
}
