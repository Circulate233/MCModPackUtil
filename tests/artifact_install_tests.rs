use std::collections::BTreeMap;
use std::io::{Cursor, Write};
use std::sync::{Arc, Barrier};
use std::time::{Duration, Instant};

use mcmpu::config::LoaderKind;
use mcmpu::project::artifact_install::{
    ArtifactDownloadRequest, ArtifactDownloader, ArtifactInstallError, ArtifactInstallProgress,
    ArtifactInstallSource, ArtifactInstaller, ArtifactInstallerImpl,
};
use mcmpu::project::launcher::{
    AssetIndexMetadata, LaunchArtifact, LaunchDescriptor, LaunchEnvironment, LaunchResolveError,
    LaunchSide, LoaderLaunchResolver, NativeArtifact, VersionJsonSource,
};
use sha1::{Digest, Sha1};

#[derive(Default)]
struct FixtureDownloader {
    bytes: BTreeMap<String, Vec<u8>>,
    calls: std::sync::atomic::AtomicUsize,
}

struct SingleVersion(String);

impl VersionJsonSource for SingleVersion {
    fn load_version_json(&self, version_id: &str) -> Result<String, LaunchResolveError> {
        if version_id == "cleanroom" {
            Ok(self.0.clone())
        } else {
            Err(LaunchResolveError::VersionNotFound {
                version_id: version_id.to_string(),
            })
        }
    }
}

impl ArtifactDownloader for FixtureDownloader {
    fn download(&self, url: &str) -> Result<Vec<u8>, ArtifactInstallError> {
        self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.bytes
            .get(url)
            .cloned()
            .ok_or_else(|| ArtifactInstallError::Download {
                url: url.to_string(),
                reason: "fixture not found".to_string(),
            })
    }
}

#[test]
fn installs_descriptor_files_and_reuses_verified_cache() {
    let temp = tempfile::tempdir().unwrap();
    let client = b"client jar".to_vec();
    let library = b"library jar".to_vec();
    let downloader = FixtureDownloader {
        bytes: BTreeMap::from([
            ("https://example.invalid/client".to_string(), client.clone()),
            (
                "https://example.invalid/library".to_string(),
                library.clone(),
            ),
        ]),
        calls: std::sync::atomic::AtomicUsize::new(0),
    };
    let descriptor = descriptor(
        Some(artifact(
            "versions/test/test.jar",
            "https://example.invalid/client",
            &client,
        )),
        vec![artifact(
            "libraries/test/library.jar",
            "https://example.invalid/library",
            &library,
        )],
        vec![],
    );
    let installer = ArtifactInstallerImpl::new(&downloader);

    let run_dir = temp.path().join("run/client");
    let cache_dir = temp.path().join("cache/runtime");
    installer
        .install(&descriptor, &run_dir, &cache_dir)
        .unwrap();
    let client_path = run_dir.join("versions/test/test.jar");
    let library_path = run_dir.join("libraries/test/library.jar");
    let client_modified = std::fs::metadata(&client_path).unwrap().modified().unwrap();
    let library_modified = std::fs::metadata(&library_path)
        .unwrap()
        .modified()
        .unwrap();
    std::thread::sleep(std::time::Duration::from_millis(1100));
    installer
        .install(&descriptor, &run_dir, &cache_dir)
        .unwrap();

    assert_eq!(
        downloader.calls.load(std::sync::atomic::Ordering::SeqCst),
        2
    );
    assert_eq!(std::fs::read(&client_path).unwrap(), client);
    assert_eq!(std::fs::read(&library_path).unwrap(), library);
    assert_eq!(
        std::fs::metadata(client_path).unwrap().modified().unwrap(),
        client_modified
    );
    assert_eq!(
        std::fs::metadata(library_path).unwrap().modified().unwrap(),
        library_modified
    );
}

#[test]
fn submits_client_and_libraries_as_one_artifact_batch() {
    struct BatchDownloader {
        bytes: BTreeMap<String, Vec<u8>>,
        batch_sizes: std::sync::Mutex<Vec<usize>>,
    }

    impl ArtifactDownloader for BatchDownloader {
        fn download(&self, url: &str) -> Result<Vec<u8>, ArtifactInstallError> {
            self.bytes
                .get(url)
                .cloned()
                .ok_or_else(|| ArtifactInstallError::Download {
                    url: url.to_string(),
                    reason: "fixture not found".to_string(),
                })
        }

        fn download_many(
            &self,
            requests: &[ArtifactDownloadRequest],
        ) -> Vec<Result<(), ArtifactInstallError>> {
            self.batch_sizes.lock().unwrap().push(requests.len());
            requests
                .iter()
                .map(|request| {
                    let bytes = self.download(&request.url)?;
                    std::fs::create_dir_all(request.target.parent().unwrap()).unwrap();
                    std::fs::write(&request.target, bytes).unwrap();
                    Ok(())
                })
                .collect()
        }
    }

    let client = b"client".to_vec();
    let first = b"first library".to_vec();
    let second = b"second library".to_vec();
    let downloader = BatchDownloader {
        bytes: BTreeMap::from([
            ("https://example.invalid/client".to_string(), client.clone()),
            ("https://example.invalid/first".to_string(), first.clone()),
            ("https://example.invalid/second".to_string(), second.clone()),
        ]),
        batch_sizes: std::sync::Mutex::new(Vec::new()),
    };
    let descriptor = descriptor(
        Some(artifact(
            "versions/test/test.jar",
            "https://example.invalid/client",
            &client,
        )),
        vec![
            artifact(
                "libraries/test/first.jar",
                "https://example.invalid/first",
                &first,
            ),
            artifact(
                "libraries/test/second.jar",
                "https://example.invalid/second",
                &second,
            ),
        ],
        vec![],
    );
    let temp = tempfile::tempdir().unwrap();

    ArtifactInstallerImpl::new(&downloader)
        .install(
            &descriptor,
            &temp.path().join("run/client"),
            &temp.path().join("cache/runtime"),
        )
        .unwrap();

    assert_eq!(*downloader.batch_sizes.lock().unwrap(), vec![3]);
}

#[test]
fn valid_run_target_does_not_read_or_create_runtime_cache() {
    let temp = tempfile::tempdir().unwrap();
    let library = b"already installed".to_vec();
    let artifact = artifact(
        "libraries/test/library.jar",
        "https://example.invalid/library",
        &library,
    );
    let descriptor = descriptor(None, vec![artifact], vec![]);
    let run_dir = temp.path().join("run/client");
    let target = run_dir.join("libraries/test/library.jar");
    std::fs::create_dir_all(target.parent().unwrap()).unwrap();
    std::fs::write(&target, &library).unwrap();
    let cache_dir = temp.path().join("cache/runtime");
    let downloader = FixtureDownloader::default();
    let mut events = Vec::new();

    ArtifactInstallerImpl::new(&downloader)
        .install_with_progress(&descriptor, &run_dir, &cache_dir, &mut |event| {
            events.push(event.clone());
        })
        .unwrap();

    assert_eq!(
        downloader.calls.load(std::sync::atomic::Ordering::SeqCst),
        0
    );
    assert!(!cache_dir.exists());
    assert!(events.iter().any(|event| matches!(
        event,
        ArtifactInstallProgress::ArtifactReady {
            path,
            source: ArtifactInstallSource::TargetHit,
            ..
        } if path == "libraries/test/library.jar"
    )));
}

#[test]
fn parsed_cleanroom_library_hits_existing_libraries_target_without_downloading() {
    let temp = tempfile::tempdir().unwrap();
    let library = b"installed cleanroom library";
    let sha1 = format!("{:x}", Sha1::digest(library));
    let profile = format!(
        r#"{{
  "id":"cleanroom",
  "mainClass":"top.outlands.foundation.boot.Foundation",
  "libraries":[{{
    "name":"com.cleanroommc:cleanroom:0.6.4-alpha",
    "downloads":{{"artifact":{{
      "path":"com/cleanroommc/cleanroom/0.6.4-alpha/cleanroom-0.6.4-alpha.jar",
      "sha1":"{sha1}",
      "size":{}
    }}}}
  }}]
}}"#,
        library.len()
    );
    let descriptor = LoaderLaunchResolver::new(SingleVersion(profile))
        .resolve_client(
            LoaderKind::Cleanroom,
            "cleanroom",
            &LaunchEnvironment::new("windows", "x86_64", 64),
        )
        .unwrap();
    let run_dir = temp.path().join("run/client");
    let target =
        run_dir.join("libraries/com/cleanroommc/cleanroom/0.6.4-alpha/cleanroom-0.6.4-alpha.jar");
    std::fs::create_dir_all(target.parent().unwrap()).unwrap();
    std::fs::write(&target, library).unwrap();
    let cache_dir = temp.path().join("cache/runtime");
    let downloader = FixtureDownloader::default();
    let mut events = Vec::new();

    ArtifactInstallerImpl::new(&downloader)
        .install_with_progress(&descriptor, &run_dir, &cache_dir, &mut |event| {
            events.push(event.clone());
        })
        .unwrap();

    assert_eq!(
        downloader.calls.load(std::sync::atomic::Ordering::SeqCst),
        0
    );
    assert!(!cache_dir.exists());
    assert!(events.iter().any(|event| matches!(
        event,
        ArtifactInstallProgress::ArtifactReady {
            path,
            source: ArtifactInstallSource::TargetHit,
            ..
        } if path == "libraries/com/cleanroommc/cleanroom/0.6.4-alpha/cleanroom-0.6.4-alpha.jar"
    )));
}

#[test]
fn concurrent_installers_download_shared_artifact_once() {
    #[derive(Clone)]
    struct SlowDownloader {
        bytes: Arc<Vec<u8>>,
        calls: Arc<std::sync::atomic::AtomicUsize>,
    }

    impl ArtifactDownloader for SlowDownloader {
        fn download(&self, _url: &str) -> Result<Vec<u8>, ArtifactInstallError> {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            std::thread::sleep(std::time::Duration::from_millis(200));
            Ok(self.bytes.as_ref().clone())
        }
    }

    let temp = tempfile::tempdir().unwrap();
    let bytes = Arc::new(b"shared library".to_vec());
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let downloader = SlowDownloader {
        bytes: Arc::clone(&bytes),
        calls: Arc::clone(&calls),
    };
    let descriptor = descriptor(
        None,
        vec![artifact(
            "libraries/test/shared.jar",
            "https://example.invalid/shared.jar",
            bytes.as_ref(),
        )],
        vec![],
    );
    let cache = temp.path().join("cache/runtime");
    let barrier = Arc::new(Barrier::new(2));

    std::thread::scope(|scope| {
        for side in ["client", "server"] {
            let barrier = Arc::clone(&barrier);
            let downloader = downloader.clone();
            let descriptor = descriptor.clone();
            let cache = cache.clone();
            let run = temp.path().join(format!("run/{side}"));
            scope.spawn(move || {
                barrier.wait();
                ArtifactInstallerImpl::new(&downloader)
                    .install(&descriptor, &run, &cache)
                    .unwrap();
            });
        }
    });

    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert_eq!(
        std::fs::read(temp.path().join("run/client/libraries/test/shared.jar")).unwrap(),
        bytes.as_ref().as_slice()
    );
    assert_eq!(
        std::fs::read(temp.path().join("run/server/libraries/test/shared.jar")).unwrap(),
        bytes.as_ref().as_slice()
    );
}

#[test]
fn reports_download_and_cache_hit_progress_for_each_runtime_artifact() {
    let temp = tempfile::tempdir().unwrap();
    let client = b"client jar".to_vec();
    let library = b"library jar".to_vec();
    let downloader = FixtureDownloader {
        bytes: BTreeMap::from([
            ("https://example.invalid/client".to_string(), client.clone()),
            (
                "https://example.invalid/library".to_string(),
                library.clone(),
            ),
        ]),
        calls: std::sync::atomic::AtomicUsize::new(0),
    };
    let descriptor = descriptor(
        Some(artifact(
            "versions/test/test.jar",
            "https://example.invalid/client",
            &client,
        )),
        vec![artifact(
            "libraries/test/library.jar",
            "https://example.invalid/library",
            &library,
        )],
        vec![],
    );
    let installer = ArtifactInstallerImpl::new(&downloader);
    let run_dir = temp.path().join("run/client");
    let cache_dir = temp.path().join("cache/runtime");
    let mut first_events = Vec::new();

    installer
        .install_with_progress(&descriptor, &run_dir, &cache_dir, &mut |event| {
            first_events.push(event.clone());
        })
        .unwrap();

    assert_eq!(
        first_events.first(),
        Some(&ArtifactInstallProgress::Starting { total: 2 })
    );
    assert_eq!(
        first_events
            .iter()
            .filter(|event| matches!(event, ArtifactInstallProgress::ArtifactReady { .. }))
            .count(),
        2
    );
    assert!(first_events.iter().all(|event| !matches!(
        event,
        ArtifactInstallProgress::ArtifactReady {
            source: ArtifactInstallSource::CacheHit,
            ..
        }
    )));

    let mut second_events = Vec::new();
    installer
        .install_with_progress(&descriptor, &run_dir, &cache_dir, &mut |event| {
            second_events.push(event.clone());
        })
        .unwrap();

    let ready: Vec<_> = second_events
        .iter()
        .filter_map(|event| match event {
            ArtifactInstallProgress::ArtifactReady {
                completed,
                total,
                path,
                source,
            } => Some((*completed, *total, path.as_str(), *source)),
            _ => None,
        })
        .collect();
    assert_eq!(
        ready,
        vec![
            (
                1,
                2,
                "versions/test/test.jar",
                ArtifactInstallSource::TargetHit,
            ),
            (
                2,
                2,
                "libraries/test/library.jar",
                ArtifactInstallSource::TargetHit,
            ),
        ]
    );
}

#[test]
fn rejects_invalid_sha1_before_writing_cache() {
    let temp = tempfile::tempdir().unwrap();
    let bytes = b"wrong".to_vec();
    let downloader = FixtureDownloader {
        bytes: BTreeMap::from([("https://example.invalid/file".to_string(), bytes)]),
        calls: std::sync::atomic::AtomicUsize::new(0),
    };
    let mut bad = artifact(
        "libraries/bad.jar",
        "https://example.invalid/file",
        b"right",
    );
    bad.sha1 = Some("0000000000000000000000000000000000000000".to_string());
    let descriptor = descriptor(None, vec![bad], vec![]);
    let error = ArtifactInstallerImpl::new(&downloader)
        .install(
            &descriptor,
            &temp.path().join("run/client"),
            &temp.path().join("cache/runtime"),
        )
        .unwrap_err();

    assert!(matches!(error, ArtifactInstallError::Sha1Mismatch { .. }));
    assert!(!temp.path().join("cache/runtime/libraries/bad.jar").exists());
    assert!(!has_part_file(temp.path()));
}

#[test]
fn installs_legacy_maven_artifact_with_checksum_sidecar_and_no_size_metadata() {
    let temp = tempfile::tempdir().unwrap();
    let bytes = b"legacy maven artifact".to_vec();
    let url = "https://repo.example/legacy.jar";
    let downloader = FixtureDownloader {
        bytes: BTreeMap::from([
            (url.to_string(), bytes.clone()),
            (
                format!("{url}.sha1"),
                format!("{:x}  legacy.jar\n", Sha1::digest(&bytes)).into_bytes(),
            ),
        ]),
        calls: std::sync::atomic::AtomicUsize::new(0),
    };
    let descriptor = descriptor(
        None,
        vec![LaunchArtifact {
            path: "legacy/example/1.0/example-1.0.jar".to_string(),
            url: Some(url.to_string()),
            sha1: None,
            size: None,
        }],
        vec![],
    );
    let installer = ArtifactInstallerImpl::new(&downloader);
    let run_dir = temp.path().join("run/client");
    let cache_dir = temp.path().join("cache/runtime");

    installer
        .install(&descriptor, &run_dir, &cache_dir)
        .unwrap();
    installer
        .install(&descriptor, &run_dir, &cache_dir)
        .unwrap();

    assert_eq!(
        std::fs::read(run_dir.join("legacy/example/1.0/example-1.0.jar")).unwrap(),
        bytes
    );
    assert_eq!(
        downloader.calls.load(std::sync::atomic::Ordering::SeqCst),
        2
    );
}

#[test]
fn repairs_only_a_corrupt_target_from_verified_cache_without_network() {
    let temp = tempfile::tempdir().unwrap();
    let client = b"client jar".to_vec();
    let library = b"library jar".to_vec();
    let downloader = FixtureDownloader {
        bytes: BTreeMap::from([
            ("https://example.invalid/client".to_string(), client.clone()),
            (
                "https://example.invalid/library".to_string(),
                library.clone(),
            ),
        ]),
        calls: std::sync::atomic::AtomicUsize::new(0),
    };
    let descriptor = descriptor(
        Some(artifact(
            "versions/test/test.jar",
            "https://example.invalid/client",
            &client,
        )),
        vec![artifact(
            "libraries/test/library.jar",
            "https://example.invalid/library",
            &library,
        )],
        vec![],
    );
    let run_dir = temp.path().join("run/client");
    let cache_dir = temp.path().join("cache/runtime");
    let installer = ArtifactInstallerImpl::new(&downloader);
    installer
        .install(&descriptor, &run_dir, &cache_dir)
        .unwrap();

    let client_path = run_dir.join("versions/test/test.jar");
    let library_path = run_dir.join("libraries/test/library.jar");
    let client_modified = std::fs::metadata(&client_path).unwrap().modified().unwrap();
    std::fs::write(&library_path, b"corrupt").unwrap();
    std::thread::sleep(std::time::Duration::from_millis(1100));

    let mut events = Vec::new();
    installer
        .install_with_progress(&descriptor, &run_dir, &cache_dir, &mut |event| {
            events.push(event.clone());
        })
        .unwrap();

    assert_eq!(
        downloader.calls.load(std::sync::atomic::Ordering::SeqCst),
        2
    );
    assert_eq!(std::fs::read(library_path).unwrap(), library);
    assert_eq!(
        std::fs::metadata(client_path).unwrap().modified().unwrap(),
        client_modified
    );
    let ready: Vec<_> = events
        .iter()
        .filter_map(|event| match event {
            ArtifactInstallProgress::ArtifactReady { path, source, .. } => {
                Some((path.as_str(), *source))
            }
            _ => None,
        })
        .collect();
    assert_eq!(
        ready,
        vec![
            ("versions/test/test.jar", ArtifactInstallSource::TargetHit),
            (
                "libraries/test/library.jar",
                ArtifactInstallSource::CacheHit
            ),
        ]
    );
}

#[test]
fn redownloads_only_an_artifact_whose_target_and_cache_are_corrupt() {
    let temp = tempfile::tempdir().unwrap();
    let client = b"client jar".to_vec();
    let library = b"library jar".to_vec();
    let downloader = FixtureDownloader {
        bytes: BTreeMap::from([
            ("https://example.invalid/client".to_string(), client.clone()),
            (
                "https://example.invalid/library".to_string(),
                library.clone(),
            ),
        ]),
        calls: std::sync::atomic::AtomicUsize::new(0),
    };
    let descriptor = descriptor(
        Some(artifact(
            "versions/test/test.jar",
            "https://example.invalid/client",
            &client,
        )),
        vec![artifact(
            "libraries/test/library.jar",
            "https://example.invalid/library",
            &library,
        )],
        vec![],
    );
    let run_dir = temp.path().join("run/client");
    let cache_dir = temp.path().join("cache/runtime");
    let installer = ArtifactInstallerImpl::new(&downloader);
    installer
        .install(&descriptor, &run_dir, &cache_dir)
        .unwrap();

    let client_path = run_dir.join("versions/test/test.jar");
    let client_modified = std::fs::metadata(&client_path).unwrap().modified().unwrap();
    std::fs::write(run_dir.join("libraries/test/library.jar"), b"bad target").unwrap();
    std::fs::write(cache_dir.join("libraries/test/library.jar"), b"bad cache").unwrap();
    std::thread::sleep(std::time::Duration::from_millis(1100));

    installer
        .install(&descriptor, &run_dir, &cache_dir)
        .unwrap();

    assert_eq!(
        downloader.calls.load(std::sync::atomic::Ordering::SeqCst),
        3
    );
    assert_eq!(
        std::fs::read(run_dir.join("libraries/test/library.jar")).unwrap(),
        library
    );
    assert_eq!(
        std::fs::metadata(client_path).unwrap().modified().unwrap(),
        client_modified
    );
}

#[test]
fn rejects_legacy_maven_artifact_when_checksum_sidecar_is_invalid() {
    let temp = tempfile::tempdir().unwrap();
    let url = "https://repo.example/legacy.jar";
    let downloader = FixtureDownloader {
        bytes: BTreeMap::from([(format!("{url}.sha1"), b"not-a-sha1".to_vec())]),
        calls: std::sync::atomic::AtomicUsize::new(0),
    };
    let descriptor = descriptor(
        None,
        vec![LaunchArtifact {
            path: "legacy/example/1.0/example-1.0.jar".to_string(),
            url: Some(url.to_string()),
            sha1: None,
            size: None,
        }],
        vec![],
    );

    let error = ArtifactInstallerImpl::new(&downloader)
        .install(
            &descriptor,
            &temp.path().join("run/client"),
            &temp.path().join("cache/runtime"),
        )
        .unwrap_err();

    assert!(matches!(
        error,
        ArtifactInstallError::InvalidChecksumSidecar { .. }
    ));
    assert!(
        !temp
            .path()
            .join("cache/runtime/legacy/example/1.0/example-1.0.jar")
            .exists()
    );
}

#[test]
fn extracts_natives_but_excludes_metadata_paths() {
    let temp = tempfile::tempdir().unwrap();
    let native_zip = zip_bytes();
    let native = NativeArtifact {
        library_name: "native".to_string(),
        classifier: "natives-windows".to_string(),
        artifact: artifact(
            "libraries/native.zip",
            "https://example.invalid/native",
            &native_zip,
        ),
        extract_excludes: vec!["META-INF/".to_string()],
    };
    let downloader = FixtureDownloader {
        bytes: BTreeMap::from([("https://example.invalid/native".to_string(), native_zip)]),
        calls: std::sync::atomic::AtomicUsize::new(0),
    };
    let descriptor = descriptor(None, vec![], vec![native]);
    let run_dir = temp.path().join("run/client");
    let cache_dir = temp.path().join("cache/runtime");
    let installer = ArtifactInstallerImpl::new(&downloader);
    installer
        .install(&descriptor, &run_dir, &cache_dir)
        .unwrap();
    let native_path = run_dir.join("natives/example.dll");
    let modified = std::fs::metadata(&native_path).unwrap().modified().unwrap();
    std::thread::sleep(std::time::Duration::from_millis(1100));
    installer
        .install(&descriptor, &run_dir, &cache_dir)
        .unwrap();

    assert_eq!(std::fs::read(&native_path).unwrap(), b"native");
    assert_eq!(
        std::fs::metadata(native_path).unwrap().modified().unwrap(),
        modified
    );
    assert_eq!(
        downloader.calls.load(std::sync::atomic::Ordering::SeqCst),
        1
    );
    assert!(
        !temp
            .path()
            .join("run/client/natives/META-INF/manifest.mf")
            .exists()
    );
}

#[test]
fn rebuilds_natives_when_an_extracted_output_is_corrupt() {
    let temp = tempfile::tempdir().unwrap();
    let native_zip = zip_bytes();
    let native = NativeArtifact {
        library_name: "native".to_string(),
        classifier: "natives-windows".to_string(),
        artifact: artifact(
            "libraries/native.zip",
            "https://example.invalid/native",
            &native_zip,
        ),
        extract_excludes: vec!["META-INF/".to_string()],
    };
    let downloader = FixtureDownloader {
        bytes: BTreeMap::from([("https://example.invalid/native".to_string(), native_zip)]),
        calls: std::sync::atomic::AtomicUsize::new(0),
    };
    let descriptor = descriptor(None, vec![], vec![native]);
    let run_dir = temp.path().join("run/client");
    let cache_dir = temp.path().join("cache/runtime");
    let installer = ArtifactInstallerImpl::new(&downloader);
    installer
        .install(&descriptor, &run_dir, &cache_dir)
        .unwrap();
    std::fs::write(run_dir.join("natives/example.dll"), b"corrupt").unwrap();

    installer
        .install(&descriptor, &run_dir, &cache_dir)
        .unwrap();

    assert_eq!(
        std::fs::read(run_dir.join("natives/example.dll")).unwrap(),
        b"native"
    );
    assert_eq!(
        downloader.calls.load(std::sync::atomic::Ordering::SeqCst),
        1
    );
}

#[test]
fn rebuilds_natives_when_exclude_rules_change_and_removes_stale_outputs() {
    let temp = tempfile::tempdir().unwrap();
    let native_zip = zip_bytes();
    let native_artifact = artifact(
        "libraries/native.zip",
        "https://example.invalid/native",
        &native_zip,
    );
    let downloader = FixtureDownloader {
        bytes: BTreeMap::from([("https://example.invalid/native".to_string(), native_zip)]),
        calls: std::sync::atomic::AtomicUsize::new(0),
    };
    let excluded = descriptor(
        None,
        vec![],
        vec![NativeArtifact {
            library_name: "native".to_string(),
            classifier: "natives-windows".to_string(),
            artifact: native_artifact.clone(),
            extract_excludes: vec!["META-INF/".to_string()],
        }],
    );
    let included = descriptor(
        None,
        vec![],
        vec![NativeArtifact {
            library_name: "native".to_string(),
            classifier: "natives-windows".to_string(),
            artifact: native_artifact,
            extract_excludes: vec![],
        }],
    );
    let run_dir = temp.path().join("run/client");
    let cache_dir = temp.path().join("cache/runtime");
    let installer = ArtifactInstallerImpl::new(&downloader);
    installer.install(&excluded, &run_dir, &cache_dir).unwrap();
    std::fs::write(run_dir.join("natives/stale.dll"), b"stale").unwrap();

    installer.install(&included, &run_dir, &cache_dir).unwrap();

    assert_eq!(
        std::fs::read(run_dir.join("natives/META-INF/manifest.mf")).unwrap(),
        b"ignored"
    );
    assert!(!run_dir.join("natives/stale.dll").exists());
    assert_eq!(
        downloader.calls.load(std::sync::atomic::Ordering::SeqCst),
        1
    );
}

#[test]
fn rejects_path_escape() {
    let temp = tempfile::tempdir().unwrap();
    let downloader = FixtureDownloader::default();
    let descriptor = descriptor(
        None,
        vec![LaunchArtifact {
            path: "../escape.jar".to_string(),
            url: Some("https://example.invalid/file".to_string()),
            sha1: None,
            size: None,
        }],
        vec![],
    );
    let error = ArtifactInstallerImpl::new(&downloader)
        .install(
            &descriptor,
            &temp.path().join("run/client"),
            &temp.path().join("cache/runtime"),
        )
        .unwrap_err();
    assert!(matches!(error, ArtifactInstallError::UnsafePath { .. }));
}

#[test]
fn installs_verified_asset_index_and_objects() {
    let temp = tempfile::tempdir().unwrap();
    let object = b"sound data".to_vec();
    let object_hash = format!("{:x}", Sha1::digest(&object));
    let index = format!(
        r#"{{"objects":{{"minecraft/sounds/test.ogg":{{"hash":"{object_hash}","size":{}}}}}}}"#,
        object.len()
    )
    .into_bytes();
    let index_hash = format!("{:x}", Sha1::digest(&index));
    let index_url = "https://example.invalid/assets.json";
    let object_url = format!(
        "https://resources.download.minecraft.net/{}/{}",
        &object_hash[..2],
        object_hash
    );
    let downloader = FixtureDownloader {
        bytes: BTreeMap::from([
            (index_url.to_string(), index.clone()),
            (object_url, object.clone()),
        ]),
        calls: std::sync::atomic::AtomicUsize::new(0),
    };
    let mut descriptor = descriptor(None, vec![], vec![]);
    descriptor.asset_index = Some(AssetIndexMetadata {
        id: "test-assets".to_string(),
        url: index_url.to_string(),
        sha1: index_hash,
        size: index.len() as u64,
        total_size: object.len() as u64,
    });
    let run_dir = temp.path().join("run/client");
    let cache_dir = temp.path().join("cache/runtime");
    let mut events = Vec::new();
    let installer = ArtifactInstallerImpl::new(&downloader);
    installer
        .install_with_progress(&descriptor, &run_dir, &cache_dir, &mut |event| {
            events.push(event.clone())
        })
        .unwrap();

    let index_path = run_dir.join("assets/indexes/test-assets.json");
    let object_path = run_dir.join(format!(
        "assets/objects/{}/{}",
        &object_hash[..2],
        object_hash
    ));
    let index_modified = std::fs::metadata(&index_path).unwrap().modified().unwrap();
    let object_modified = std::fs::metadata(&object_path).unwrap().modified().unwrap();
    std::thread::sleep(std::time::Duration::from_millis(1100));
    installer
        .install(&descriptor, &run_dir, &cache_dir)
        .unwrap();

    assert_eq!(std::fs::read(&object_path).unwrap(), object);
    assert!(index_path.is_file());
    assert_eq!(
        std::fs::metadata(index_path).unwrap().modified().unwrap(),
        index_modified
    );
    assert_eq!(
        std::fs::metadata(object_path).unwrap().modified().unwrap(),
        object_modified
    );
    assert_eq!(
        downloader.calls.load(std::sync::atomic::Ordering::SeqCst),
        2
    );
    assert_eq!(
        events,
        vec![
            ArtifactInstallProgress::Starting { total: 1 },
            ArtifactInstallProgress::TotalUpdated { total: 2 },
            ArtifactInstallProgress::ArtifactReady {
                completed: 1,
                total: 2,
                path: "assets/indexes/test-assets.json".to_string(),
                source: ArtifactInstallSource::Downloaded,
            },
            ArtifactInstallProgress::ArtifactReady {
                completed: 2,
                total: 2,
                path: format!("assets/objects/{}/{}", &object_hash[..2], object_hash),
                source: ArtifactInstallSource::Downloaded,
            },
        ]
    );
}

#[test]
fn submits_asset_objects_as_one_batch_with_monotonic_progress() {
    struct BatchDownloader {
        bytes: BTreeMap<String, Vec<u8>>,
        batch_sizes: std::sync::Mutex<Vec<usize>>,
    }

    impl ArtifactDownloader for BatchDownloader {
        fn download(&self, url: &str) -> Result<Vec<u8>, ArtifactInstallError> {
            self.bytes
                .get(url)
                .cloned()
                .ok_or_else(|| ArtifactInstallError::Download {
                    url: url.to_string(),
                    reason: "fixture not found".to_string(),
                })
        }

        fn download_many(
            &self,
            requests: &[ArtifactDownloadRequest],
        ) -> Vec<Result<(), ArtifactInstallError>> {
            self.batch_sizes.lock().unwrap().push(requests.len());
            requests
                .iter()
                .map(|request| {
                    let bytes = self.download(&request.url)?;
                    std::fs::create_dir_all(request.target.parent().unwrap()).unwrap();
                    std::fs::write(&request.target, bytes).unwrap();
                    Ok(())
                })
                .collect()
        }
    }

    let temp = tempfile::tempdir().unwrap();
    let (metadata, downloads) = asset_fixture(12);
    let downloader = BatchDownloader {
        bytes: downloads,
        batch_sizes: std::sync::Mutex::new(Vec::new()),
    };
    let mut descriptor = descriptor(None, vec![], vec![]);
    descriptor.asset_index = Some(metadata);
    let mut events = Vec::new();

    ArtifactInstallerImpl::new(&downloader)
        .install_with_progress(
            &descriptor,
            &temp.path().join("run/client"),
            &temp.path().join("cache/runtime"),
            &mut |event| events.push(event.clone()),
        )
        .unwrap();

    assert_eq!(*downloader.batch_sizes.lock().unwrap(), vec![1, 12]);
    let ready: Vec<_> = events
        .iter()
        .filter_map(|event| match event {
            ArtifactInstallProgress::ArtifactReady {
                completed, total, ..
            } => Some((*completed, *total)),
            _ => None,
        })
        .collect();
    assert_eq!(ready.len(), 13);
    assert!(ready.windows(2).all(|pair| pair[0].0 < pair[1].0));
    assert_eq!(ready.last(), Some(&(13, 13)));
}

#[test]
fn installs_duplicate_asset_hash_only_once() {
    let temp = tempfile::tempdir().unwrap();
    let object = b"shared asset object".to_vec();
    let hash = format!("{:x}", Sha1::digest(&object));
    let uppercase_hash = hash.to_ascii_uppercase();
    let index = format!(
        r#"{{"objects":{{"first":{{"hash":"{hash}","size":{size}}},"second":{{"hash":"{uppercase_hash}","size":{size}}}}}}}"#,
        size = object.len()
    )
    .into_bytes();
    let index_url = "https://example.invalid/duplicate-assets.json";
    let object_url = format!(
        "https://resources.download.minecraft.net/{}/{}",
        &hash[..2],
        hash
    );
    let downloader = FixtureDownloader {
        bytes: BTreeMap::from([(index_url.to_string(), index.clone()), (object_url, object)]),
        calls: std::sync::atomic::AtomicUsize::new(0),
    };
    let mut descriptor = descriptor(None, vec![], vec![]);
    descriptor.asset_index = Some(AssetIndexMetadata {
        id: "duplicate-assets".to_string(),
        url: index_url.to_string(),
        sha1: format!("{:x}", Sha1::digest(&index)),
        size: index.len() as u64,
        total_size: 0,
    });
    let mut events = Vec::new();

    ArtifactInstallerImpl::new(&downloader)
        .install_with_progress(
            &descriptor,
            &temp.path().join("run/client"),
            &temp.path().join("cache/runtime"),
            &mut |event| events.push(event.clone()),
        )
        .unwrap();

    assert_eq!(
        downloader.calls.load(std::sync::atomic::Ordering::SeqCst),
        2
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, ArtifactInstallProgress::ArtifactReady { .. }))
            .count(),
        2
    );
    assert_eq!(
        events.iter().find_map(|event| match event {
            ArtifactInstallProgress::TotalUpdated { total } => Some(*total),
            _ => None,
        }),
        Some(2)
    );
}

#[test]
fn asset_batch_failure_returns_original_error_without_starting_later_objects() {
    struct FailingDownloader {
        bytes: BTreeMap<String, Vec<u8>>,
        failed_url: String,
        object_calls: std::sync::atomic::AtomicUsize,
        batch_sizes: std::sync::Mutex<Vec<usize>>,
    }

    impl ArtifactDownloader for FailingDownloader {
        fn download(&self, url: &str) -> Result<Vec<u8>, ArtifactInstallError> {
            if url.contains("resources.download.minecraft.net") {
                self.object_calls
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                if url == self.failed_url {
                    return Err(ArtifactInstallError::Download {
                        url: url.to_string(),
                        reason: "intentional asset failure".to_string(),
                    });
                }
            }
            self.bytes
                .get(url)
                .cloned()
                .ok_or_else(|| ArtifactInstallError::Download {
                    url: url.to_string(),
                    reason: "fixture not found".to_string(),
                })
        }

        fn download_many(
            &self,
            requests: &[ArtifactDownloadRequest],
        ) -> Vec<Result<(), ArtifactInstallError>> {
            self.batch_sizes.lock().unwrap().push(requests.len());
            let mut failed = false;
            requests
                .iter()
                .map(|request| {
                    if failed {
                        return Err(ArtifactInstallError::Download {
                            url: request.url.clone(),
                            reason: "cancelled after earlier batch failure".to_string(),
                        });
                    }
                    match self.download(&request.url) {
                        Ok(bytes) => {
                            std::fs::create_dir_all(request.target.parent().unwrap()).unwrap();
                            std::fs::write(&request.target, bytes).unwrap();
                            Ok(())
                        }
                        Err(error) => {
                            failed = true;
                            Err(error)
                        }
                    }
                })
                .collect()
        }
    }

    let temp = tempfile::tempdir().unwrap();
    let (metadata, downloads) = asset_fixture(64);
    let failed_url = downloads
        .keys()
        .find(|url| url.contains("resources.download.minecraft.net"))
        .unwrap()
        .clone();
    let downloader = FailingDownloader {
        bytes: downloads,
        failed_url: failed_url.clone(),
        object_calls: std::sync::atomic::AtomicUsize::new(0),
        batch_sizes: std::sync::Mutex::new(Vec::new()),
    };
    let mut descriptor = descriptor(None, vec![], vec![]);
    descriptor.asset_index = Some(metadata);
    let started = Instant::now();

    let error = ArtifactInstallerImpl::new(&downloader)
        .install(
            &descriptor,
            &temp.path().join("run/client"),
            &temp.path().join("cache/runtime"),
        )
        .unwrap_err();

    assert!(matches!(
        error,
        ArtifactInstallError::Download { url, reason }
            if url == failed_url && reason == "intentional asset failure"
    ));
    assert!(started.elapsed() < Duration::from_secs(2));
    assert_eq!(*downloader.batch_sizes.lock().unwrap(), vec![1, 64]);
    assert_eq!(
        downloader
            .object_calls
            .load(std::sync::atomic::Ordering::SeqCst),
        1
    );
}

#[test]
fn restores_asset_objects_from_cache_without_network_downloads() {
    let temp = tempfile::tempdir().unwrap();
    let (metadata, downloads) = asset_fixture(8);
    let downloader = FixtureDownloader {
        bytes: downloads,
        calls: std::sync::atomic::AtomicUsize::new(0),
    };
    let mut descriptor = descriptor(None, vec![], vec![]);
    descriptor.asset_index = Some(metadata);
    let run_dir = temp.path().join("run/client");
    let cache_dir = temp.path().join("cache/runtime");
    let installer = ArtifactInstallerImpl::new(&downloader);
    installer
        .install(&descriptor, &run_dir, &cache_dir)
        .unwrap();
    let calls_after_first = downloader.calls.load(std::sync::atomic::Ordering::SeqCst);
    std::fs::remove_dir_all(run_dir.join("assets/objects")).unwrap();
    let mut events = Vec::new();

    installer
        .install_with_progress(&descriptor, &run_dir, &cache_dir, &mut |event| {
            events.push(event.clone())
        })
        .unwrap();

    assert_eq!(
        downloader.calls.load(std::sync::atomic::Ordering::SeqCst),
        calls_after_first
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(
                event,
                ArtifactInstallProgress::ArtifactReady {
                    source: ArtifactInstallSource::CacheHit,
                    path,
                    ..
                } if path.starts_with("assets/objects/")
            ))
            .count(),
        8
    );
}

#[test]
fn accepts_empty_asset_index_without_starting_object_workers() {
    let temp = tempfile::tempdir().unwrap();
    let index = br#"{"objects":{}}"#.to_vec();
    let index_url = "https://example.invalid/empty-assets.json";
    let downloader = FixtureDownloader {
        bytes: BTreeMap::from([(index_url.to_string(), index.clone())]),
        calls: std::sync::atomic::AtomicUsize::new(0),
    };
    let mut descriptor = descriptor(None, vec![], vec![]);
    descriptor.asset_index = Some(AssetIndexMetadata {
        id: "empty-assets".to_string(),
        url: index_url.to_string(),
        sha1: format!("{:x}", Sha1::digest(&index)),
        size: index.len() as u64,
        total_size: 0,
    });
    let mut events = Vec::new();

    ArtifactInstallerImpl::new(&downloader)
        .install_with_progress(
            &descriptor,
            &temp.path().join("run/client"),
            &temp.path().join("cache/runtime"),
            &mut |event| events.push(event.clone()),
        )
        .unwrap();

    assert_eq!(
        downloader.calls.load(std::sync::atomic::Ordering::SeqCst),
        1
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, ArtifactInstallProgress::ArtifactReady { .. }))
            .count(),
        1
    );
    assert_eq!(
        events.last(),
        Some(&ArtifactInstallProgress::ArtifactReady {
            completed: 1,
            total: 1,
            path: "assets/indexes/empty-assets.json".to_string(),
            source: ArtifactInstallSource::Downloaded,
        })
    );
}

fn asset_fixture(count: usize) -> (AssetIndexMetadata, BTreeMap<String, Vec<u8>>) {
    let index_url = "https://example.invalid/generated-assets.json";
    let mut objects = serde_json::Map::new();
    let mut downloads = BTreeMap::new();
    for index in 0..count {
        let bytes = format!("asset-{index}").into_bytes();
        let hash = format!("{:x}", Sha1::digest(&bytes));
        objects.insert(
            format!("minecraft/generated/{index}"),
            serde_json::json!({ "hash": hash, "size": bytes.len() }),
        );
        downloads.insert(
            format!(
                "https://resources.download.minecraft.net/{}/{}",
                &hash[..2],
                hash
            ),
            bytes,
        );
    }
    let index = serde_json::to_vec(&serde_json::json!({ "objects": objects })).unwrap();
    downloads.insert(index_url.to_string(), index.clone());
    (
        AssetIndexMetadata {
            id: "generated-assets".to_string(),
            url: index_url.to_string(),
            sha1: format!("{:x}", Sha1::digest(&index)),
            size: index.len() as u64,
            total_size: 0,
        },
        downloads,
    )
}

fn has_part_file(root: &std::path::Path) -> bool {
    let mut pending = vec![root.to_path_buf()];
    while let Some(directory) = pending.pop() {
        let Ok(entries) = std::fs::read_dir(directory) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                pending.push(path);
            } else if path
                .extension()
                .is_some_and(|extension| extension == "part")
            {
                return true;
            }
        }
    }
    false
}

fn descriptor(
    client_jar: Option<LaunchArtifact>,
    libraries: Vec<LaunchArtifact>,
    natives: Vec<NativeArtifact>,
) -> LaunchDescriptor {
    LaunchDescriptor {
        loader: LoaderKind::Forge,
        side: LaunchSide::Client,
        version_id: "test".to_string(),
        inheritance_chain: vec!["test".to_string()],
        main_class: "example.Main".to_string(),
        libraries,
        classpath: vec![],
        client_jar,
        jvm_arguments: vec![],
        game_arguments: vec![],
        assets: None,
        asset_index: None,
        natives,
        java_version_major: Some(8),
    }
}

fn artifact(path: &str, url: &str, bytes: &[u8]) -> LaunchArtifact {
    LaunchArtifact {
        path: path.to_string(),
        url: Some(url.to_string()),
        sha1: Some(format!("{:x}", Sha1::digest(bytes))),
        size: Some(bytes.len() as u64),
    }
}

fn zip_bytes() -> Vec<u8> {
    let mut bytes = Cursor::new(Vec::new());
    {
        let mut zip = zip::ZipWriter::new(&mut bytes);
        zip.start_file("example.dll", zip::write::SimpleFileOptions::default())
            .unwrap();
        zip.write_all(b"native").unwrap();
        zip.start_file(
            "META-INF/manifest.mf",
            zip::write::SimpleFileOptions::default(),
        )
        .unwrap();
        zip.write_all(b"ignored").unwrap();
        zip.finish().unwrap();
    }
    bytes.into_inner()
}
