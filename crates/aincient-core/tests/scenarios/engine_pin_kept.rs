//! Scenario 27 — turning the engine off keeps a local engine build it can't
//! download again (C4 in 1c2a6b4). `disable` drops the local name
//! `protect_image` gave the engine (`ghcr.io/aincient-labs/atelier-engine:
//! pinned-<project>`), but only when the image can be downloaded again or
//! keeps another name: removing a local build's last name deletes it for good.
//!
//! Linux CI, 2026-11-15: on Docker's containerd image store (the default for
//! a fresh Docker Engine 29, and Docker Desktop's) a local build lists a
//! `RepoDigests` entry under every name it has, so the old "has a registry
//! digest" test said such a build could be downloaded again, and `disable`
//! would have deleted it. Whether an image was pulled is now read from what
//! Docker recorded about where it came from (`aincient_core::image_origin`).
//! Run on both stores in CI.
//!
//! Also checks that reading on real Docker: a local build is never
//! downloadable, the scenario's pulled database image is.

use std::time::Instant;

use aincient_core::engine::{self, pinned_tag, protect_image};
use aincient_core::image_origin::{self, same_repository};
use aincient_core::stack::{InstallOptions, COMPOSE_TEMPLATE};

use crate::harness::{docker, try_docker, Site, SCN_DB_IMAGE};

#[test]
#[ignore = "real Docker: cargo test -p aincient-core --test scenarios -- --ignored --test-threads=1"]
fn engine_off_keeps_a_local_engine_build_that_cant_be_downloaded_again() {
    let t = Instant::now();
    let store = image_origin::store();
    eprintln!("image store: {store:?}");

    // What Docker recorded, read the Manager's way, on this store.
    let mut site = Site::new("engine-pin");
    let (build, build_id) = site.build_app("engine", "e");
    let local = image_origin::inspect(&build).expect("the local build is here");
    assert_eq!(local.downloadable(store), Some(false), "a local build");
    assert_eq!(local.registry_digest(store), None, "has no registry digest");
    if try_docker(&["image", "inspect", SCN_DB_IMAGE]).is_err() {
        docker(&["pull", "-q", SCN_DB_IMAGE]);
    }
    let pulled = image_origin::inspect(SCN_DB_IMAGE).expect("the database image");
    assert_eq!(
        pulled.downloadable(store),
        Some(true),
        "{SCN_DB_IMAGE} was pulled"
    );
    assert!(pulled
        .pulled_from(store)
        .unwrap_or_default()
        .iter()
        .any(|r| same_repository(r, "postgres")));
    assert!(pulled.registry_digest(store).is_some());

    // A site on the shipped templates (engine::disable refuses others), whose
    // engine is the local build, known only by the pinned name.
    site.stack
        .ensure_scaffold_with(
            &InstallOptions {
                image: Some(build.clone()),
                http_port: Some(1),
            },
            || Some(false),
        )
        .expect("scaffold");
    std::fs::write(site.stack.compose_path(), COMPOSE_TEMPLATE).expect("compose.yaml");
    protect_image(&site.stack, &build_id);
    let pin = pinned_tag(site.project());
    site.owns_tag(&pin);
    docker(&["image", "rm", &build]);
    assert_eq!(
        docker(&["image", "inspect", "-f", "{{.Id}}", &pin]),
        build_id,
        "the pin is the build's only name"
    );

    engine::disable(&site.stack).expect("engine off");
    assert_eq!(
        try_docker(&["image", "inspect", "-f", "{{.Id}}", &pin]).as_deref(),
        Ok(build_id.as_str()),
        "the local engine build and its name are kept"
    );
    eprintln!("scenario 27 took {:.0?}", t.elapsed());
}
