use std::path::Path;

use serde_yaml_ng::Value;

const CHECKOUT_SHA: &str = "11bd71901bbe5b1630ceea73d27597364c9af683";
const UPLOAD_ARTIFACT_SHA: &str = "ea165f8d65b6e75b540449e92b4886f43607fa02";
const DOWNLOAD_ARTIFACT_SHA: &str = "d3f86a106a0bac45b974a628896c90dbdf5c8093";
const RUST_VERSION: &str = "1.94.1";

#[test]
fn ci_actions_are_known_and_pinned_to_immutable_commits() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let workflow: Value =
        serde_yaml_ng::from_slice(&std::fs::read(root.join(".github/workflows/ci.yml")).unwrap())
            .unwrap();
    let mut uses = Vec::new();
    collect_uses(&workflow, &mut uses);

    assert!(!uses.is_empty());
    for action in uses {
        let (name, revision) = action
            .rsplit_once('@')
            .unwrap_or_else(|| panic!("action is not pinned: {action}"));
        assert_eq!(
            revision.len(),
            40,
            "action is not pinned by commit: {action}"
        );
        assert!(
            revision.bytes().all(|byte| byte.is_ascii_hexdigit()),
            "action revision is not a commit SHA: {action}"
        );
        match name {
            "actions/checkout" => assert_eq!(revision, CHECKOUT_SHA),
            "actions/upload-artifact" => assert_eq!(revision, UPLOAD_ARTIFACT_SHA),
            "actions/download-artifact" => assert_eq!(revision, DOWNLOAD_ARTIFACT_SHA),
            other => panic!("unreviewed third-party action: {other}"),
        }
    }

    let permissions = workflow
        .get("permissions")
        .and_then(Value::as_mapping)
        .expect("workflow must declare least-privilege permissions");
    assert_eq!(
        permissions
            .get(Value::String("contents".to_string()))
            .and_then(Value::as_str),
        Some("read")
    );
}

#[test]
fn tag_release_is_the_only_job_with_repository_write_permission() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let workflow: Value =
        serde_yaml_ng::from_slice(&std::fs::read(root.join(".github/workflows/ci.yml")).unwrap())
            .unwrap();
    let jobs = workflow
        .get("jobs")
        .and_then(Value::as_mapping)
        .expect("workflow must contain jobs");
    let release = jobs
        .get(Value::String("release".to_string()))
        .and_then(Value::as_mapping)
        .expect("workflow must contain a release job");
    assert_eq!(
        release
            .get(Value::String("if".to_string()))
            .and_then(Value::as_str),
        Some("needs.version-policy.outputs.release-version != ''"),
        "release must be gated by a validated v-prefixed tag"
    );
    assert_eq!(
        release
            .get(Value::String("permissions".to_string()))
            .and_then(Value::as_mapping)
            .and_then(|permissions| permissions.get(Value::String("contents".to_string())))
            .and_then(Value::as_str),
        Some("write")
    );
    for (name, job) in jobs {
        if name.as_str() == Some("release") {
            continue;
        }
        assert_ne!(
            job.as_mapping()
                .and_then(|mapping| mapping.get(Value::String("permissions".to_string())))
                .and_then(Value::as_mapping)
                .and_then(|permissions| permissions.get(Value::String("contents".to_string())))
                .and_then(Value::as_str),
            Some("write"),
            "only release may write repository contents"
        );
    }
}

#[test]
fn repository_toolchain_is_exact_and_ci_uses_the_repository_toolchain() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let toolchain: toml::Value =
        toml::from_str(&std::fs::read_to_string(root.join("rust-toolchain.toml")).unwrap())
            .unwrap();
    assert_eq!(
        toolchain["toolchain"]["channel"].as_str(),
        Some(RUST_VERSION)
    );

    let workflow: Value =
        serde_yaml_ng::from_slice(&std::fs::read(root.join(".github/workflows/ci.yml")).unwrap())
            .unwrap();
    let mut uses = Vec::new();
    collect_uses(&workflow, &mut uses);
    assert!(
        uses.iter().all(|action| !action.contains("rust-toolchain")),
        "CI must not override the repository's exact Rust toolchain"
    );
}

#[test]
fn release_workflow_is_self_contained_and_builds_the_validated_version() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let workflow_text =
        std::fs::read_to_string(root.join(".github/workflows/release.yml")).unwrap();
    let workflow: Value = serde_yaml_ng::from_str(&workflow_text).unwrap();
    let jobs = workflow
        .get("jobs")
        .and_then(Value::as_mapping)
        .expect("release workflow must contain jobs");

    let build_steps = jobs
        .get(Value::String("build".to_string()))
        .and_then(Value::as_mapping)
        .and_then(|job| job.get(Value::String("steps".to_string())))
        .and_then(Value::as_sequence)
        .expect("release build job must contain steps");
    let build_step = build_steps
        .iter()
        .find(|step| step.get("name").and_then(Value::as_str) == Some("Build release binary"))
        .expect("release build must have a binary build step");
    assert_eq!(
        build_step
            .get("env")
            .and_then(|env| env.get("MCMODPACKUTIL_BUILD_VERSION"))
            .and_then(Value::as_str),
        Some("${{ needs.validate.outputs.version }}"),
        "the binary build must use the validated release version override"
    );
    assert!(!workflow_text.contains("MCSDT_RELEASE_VERSION"));

    let mut run_commands = Vec::new();
    collect_run_commands(&workflow, &mut run_commands);
    assert!(
        run_commands
            .iter()
            .all(|command| !command.contains("./.github/scripts/")),
        "release workflow must not call helper scripts absent from this repository"
    );

    let mut uses = Vec::new();
    collect_uses(&workflow, &mut uses);
    assert!(!uses.is_empty());
    for action in uses {
        let (_, revision) = action
            .rsplit_once('@')
            .unwrap_or_else(|| panic!("release action is not pinned: {action}"));
        assert_eq!(
            revision.len(),
            40,
            "action is not pinned by commit: {action}"
        );
        assert!(
            revision.bytes().all(|byte| byte.is_ascii_hexdigit()),
            "action revision is not a commit SHA: {action}"
        );
    }
}

fn collect_uses<'a>(value: &'a Value, output: &mut Vec<&'a str>) {
    match value {
        Value::Mapping(mapping) => {
            for (key, value) in mapping {
                if key.as_str() == Some("uses") {
                    output.push(value.as_str().expect("uses must be a string"));
                }
                collect_uses(value, output);
            }
        }
        Value::Sequence(values) => {
            for value in values {
                collect_uses(value, output);
            }
        }
        _ => {}
    }
}

fn collect_run_commands<'a>(value: &'a Value, output: &mut Vec<&'a str>) {
    match value {
        Value::Mapping(mapping) => {
            for (key, value) in mapping {
                if key.as_str() == Some("run") {
                    output.push(value.as_str().expect("run must be a string"));
                }
                collect_run_commands(value, output);
            }
        }
        Value::Sequence(values) => {
            for value in values {
                collect_run_commands(value, output);
            }
        }
        _ => {}
    }
}
