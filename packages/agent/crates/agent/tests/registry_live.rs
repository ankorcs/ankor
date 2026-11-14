//! The registry reads against real registries: ghcr.io and Docker Hub, each with its token service.
//! Ignored unless run with `--run-ignored only`.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing, clippy::unimplemented, reason = "a test fails by crashing")]

use ankor_agent::registry_tags::{HttpRegistry, list_tags, tag_digest};

#[tokio::test]
#[ignore = "reaches ghcr.io and Docker Hub"]
async fn lists_tags_and_reads_a_digest_from_ghcr_and_docker_hub() {
    let registry = HttpRegistry::new();

    for (reference, tag) in [("ghcr.io/linuxserver/nginx:latest", "latest"), ("alpine:3.21", "3.21")] {
        let tags = list_tags(reference, &registry, &ankor_agent::logger::Logger::null()).await.unwrap();

        assert!(tags.iter().any(|each| each == tag), "{reference}: {} tags", tags.len());
        assert!(tag_digest(reference, &registry).await.unwrap().starts_with("sha256:"), "{reference}");
    }
}
