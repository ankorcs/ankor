//! The Docker service against a real daemon. Ignored unless run with `--run-ignored only`.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing, clippy::unimplemented, reason = "a test fails by crashing")]
#![allow(clippy::disallowed_methods, reason = "a test locks its fixtures and fails on a poisoned lock")]

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ankor_agent::container_spec::{ContainerRole, LabelInput, container_labels};
use ankor_agent::docker::{ContainerHooks, DockerPort as _, DockerService, NetworkMode, StartContainerSpec};
use ankor_agent::docker_api::DockerApi;
use ankor_agent::logger::Logger;
use tokio::sync::oneshot;

const IMAGE: &str = "alpine:3.21";

#[tokio::test]
#[ignore = "needs a Docker daemon"]
async fn a_started_container_reports_its_lines_and_exit() {
    let docker = DockerService::new(DockerApi::default(), Logger::null());

    docker.require_supported_engine().await.unwrap();
    docker.pull_image(IMAGE).await.unwrap();

    let lines = Arc::new(Mutex::new(Vec::new()));
    let (seen, by_exit) = (Arc::clone(&lines), Arc::clone(&lines));
    let (exited, exit) = oneshot::channel();
    let exited = Mutex::new(Some(exited));
    // The lines are read as the exit is reported, so a line that comes after it fails the test.
    let hooks = ContainerHooks {
        on_line: Arc::new(move |line| seen.lock().unwrap().push(line)),
        on_exit: Arc::new(move |code| {
            if let Some(exited) = exited.lock().unwrap().take() {
                let _ = exited.send((code, by_exit.lock().unwrap().clone()));
            }
        }),
    };
    let slot = format!("rs{}", std::process::id());
    let mut labels = container_labels(ContainerRole::Probe, &LabelInput { slot_id: Some(&slot), idle_shutdown_seconds: Some(600), ..LabelInput::default() });

    labels.insert("ankor.test".to_owned(), slot.clone());

    let spec = StartContainerSpec {
        name: format!("ankor-{slot}-probe-test"),
        image: IMAGE.to_owned(),
        env: vec!["GREETING=hi".to_owned()],
        labels,
        network_mode: NetworkMode::Bridge,
        cap_drop: vec!["ALL".to_owned()],
        security_opt: vec!["no-new-privileges=true".to_owned()],
        pids_limit: 64,
        memory_bytes: Some(64 << 20),
        cpu_shares: None,
        auto_remove: false,
        volumes: Vec::new(),
        binds: Vec::new(),
        tmpfs: Vec::new(),
        cmd: Some(vec!["sh".to_owned(), "-c".to_owned(), "echo \"$GREETING\"; printf '\\033[32mgreen\\033[0m\\r\\n'; printf 'no newline'; exit 7".to_owned()]),
        entrypoint: None,
    };
    let id = docker.start_container(&spec, hooks).await.unwrap();

    let (code, lines_by_exit) = tokio::time::timeout(Duration::from_secs(20), exit).await.unwrap().unwrap();

    assert_eq!(code, Some(7));
    // Colour escapes stay for the log viewer; the carriage return goes.
    assert_eq!(lines_by_exit, ["hi", "\u{1b}[32mgreen\u{1b}[0m", "no newline"]);

    // Docker answers the wait once the container is stopped, but its container list catches up a
    // moment later, so a list right after the exit can still say running.
    let adopted = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let listed = docker.list_ankor_containers().await.unwrap().into_iter().find(|container| container.id == id).unwrap();

            if !listed.running {
                return listed;
            }

            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("the list reports the container stopped");

    assert_eq!(adopted.slot_id.as_deref(), Some(slot.as_str()));
    assert_eq!(adopted.role, Some(ContainerRole::Probe));
    assert_eq!(adopted.idle_shutdown_seconds, Some(600));

    let swept = docker.sweep_helper_containers().await.unwrap();

    assert!(swept.contains(&spec.name), "{swept:?}");
    assert!(docker.image_id("ghcr.io/ankorcs/not-here:0").await.unwrap().is_none());
    assert!(docker.image_id(IMAGE).await.unwrap().is_some());
    // Docker answers a stop of a stopped container with 304, and a removal of a removed one with 404.
    docker.stop_container(&id, 1).await.unwrap();
    docker.remove_container(&id, true).await.unwrap();
    docker.remove_container(&id, true).await.unwrap();
    assert!(docker.list_ankor_containers().await.unwrap().iter().all(|container| container.id != id));

    let _: BTreeMap<String, String> = docker.image_defaults(IMAGE).await.unwrap().labels;
    let error = docker.pull_image("ghcr.io/ankorcs/does-not-exist:0").await.unwrap_err();

    assert!(error.to_string().contains("does-not-exist") || error.status().is_some(), "{error}");
}

/// The exit wait is registered before the start, so a container that Docker removes as it exits
/// still gives its exit code.
#[tokio::test]
#[ignore = "needs a Docker daemon"]
async fn an_auto_removed_container_keeps_its_exit_code() {
    let docker = DockerService::new(DockerApi::default(), Logger::null());

    docker.pull_image(IMAGE).await.unwrap();

    let (exited, exit) = oneshot::channel();
    let exited = Mutex::new(Some(exited));
    let hooks = ContainerHooks {
        on_line: Arc::new(|_| {}),
        on_exit: Arc::new(move |code| {
            if let Some(exited) = exited.lock().unwrap().take() {
                let _ = exited.send(code);
            }
        }),
    };
    let slot = format!("ar{}", std::process::id());
    let spec = StartContainerSpec {
        name: format!("ankor-{slot}-removed-test"),
        image: IMAGE.to_owned(),
        labels: container_labels(ContainerRole::Probe, &LabelInput { slot_id: Some(&slot), ..LabelInput::default() }),
        auto_remove: true,
        cmd: Some(vec!["sh".to_owned(), "-c".to_owned(), "exit 5".to_owned()]),
        ..ankor_agent::testing::empty_spec()
    };

    docker.start_container(&spec, hooks).await.unwrap();
    assert_eq!(tokio::time::timeout(Duration::from_secs(20), exit).await.unwrap().unwrap(), Some(5));
}

#[tokio::test]
#[ignore = "needs a Docker daemon"]
async fn copies_a_folder_out_of_an_image_keeping_modes_and_symlinks() {
    use std::os::unix::fs::PermissionsExt as _;

    let docker = DockerService::new(DockerApi::default(), Logger::null());

    docker.pull_image(IMAGE).await.unwrap();

    let dir = tempfile::tempdir().unwrap();
    let slot = format!("cp{}", std::process::id());
    let mut labels = container_labels(ContainerRole::Overlay, &LabelInput::default());

    labels.insert("ankor.test".to_owned(), slot.clone());

    let spec = StartContainerSpec { name: format!("ankor-{slot}-copy-test"), image: IMAGE.to_owned(), labels, ..ankor_agent::testing::empty_spec() };

    docker.copy_from_image(&spec, "/bin", dir.path()).await.unwrap();

    let bin = dir.path().join("bin");
    let busybox = std::fs::symlink_metadata(bin.join("busybox")).unwrap();
    let ls = std::fs::symlink_metadata(bin.join("ls")).unwrap();

    assert!(busybox.is_file());
    assert_eq!(busybox.permissions().mode() & 0o7777, 0o755);
    assert!(ls.file_type().is_symlink());
    assert_eq!(std::fs::read_link(bin.join("ls")).unwrap(), std::path::Path::new("/bin/busybox"));

    let error = docker.copy_from_image(&spec, "/no/such/folder", dir.path()).await.unwrap_err();

    assert_eq!(error.status(), Some(404));
    let left = DockerApi::default().list_containers(true, None).await.unwrap();

    assert!(left.iter().all(|container| container.names.iter().all(|name| !name.contains(&slot))), "the containers are removed");
}
