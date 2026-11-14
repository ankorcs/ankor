//! The addon overlay of a real cs2 image. Ignored unless run with `--run-ignored only`.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing, clippy::unimplemented, reason = "a test fails by crashing")]

use std::sync::Arc;

use ankor_agent::docker::DockerService;
use ankor_agent::docker_api::DockerApi;
use ankor_agent::logger::Logger;

/// Extracts the overlay of the cs2 image in `ANKOR_CS2_IMAGE`, into `ANKOR_OVERLAY_DATA_DIR` when
/// set so it can be compared afterwards.
#[tokio::test]
#[ignore = "needs a Docker daemon and a cs2 image in ANKOR_CS2_IMAGE"]
async fn extracts_the_addon_overlay_of_a_real_cs2_image() {
    let image = std::env::var("ANKOR_CS2_IMAGE").expect("ANKOR_CS2_IMAGE names the cs2 image to extract");
    let scratch = tempfile::tempdir().unwrap();
    let data_dir = std::env::var("ANKOR_OVERLAY_DATA_DIR").map_or_else(|_| scratch.path().to_owned(), std::path::PathBuf::from);
    let docker: Arc<dyn ankor_agent::docker::DockerPort> = Arc::new(DockerService::new(DockerApi::default(), Logger::null()));
    let store = ankor_agent::overlay::OverlayStore::new(docker, &data_dir, Logger::null());
    let overlay = store.ensure(&image).await.unwrap();

    assert!(overlay.dir.join("addons").is_dir());
    assert!(overlay.dir.join(ankor_agent::overlay::COMPLETE_MARKER).is_file());

    for (rel_path, _) in ankor_agent::host_layout::overlay_mountpoints() {
        assert!(overlay.dir.join(&rel_path).exists(), "{rel_path}");
    }
}
