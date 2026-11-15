//! Scenario 7 — free up space never takes what a registered site needs (C1 in
//! 1c2a6b4, the automatic pass narrowed in 1078041): a stopped site's image, a
//! site pinned by digest, and an engine site's `pinned-<project>` name must all
//! count as needed, read from the real registry and the sites' real files, so
//! clean-up never makes a site download its own image to start (or lose one
//! it can't download). An unrelated generic image stays too.
//!
//! The registry is the scenario's own (`harness::user_home`), never the
//! owner's. Clean-up is run in selection mode only (`cleanup::find`), never
//! `free`: it lists every image on this computer, the owner's included.
//!
//! A local build tagged under Atelier's own repository must never be picked
//! either: it can't be downloaded again (on the containerd image store it
//! lists a registry digest all the same).
//!
//! Not covered, and why: an image clean-up may REMOVE must have been pulled
//! from Atelier's own repositories (`ghcr.io/aincient-labs/…`); a local build
//! wasn't, and a test can't make one without publishing there.
//! So the "superseded one is removed" half is left to the unit tests over
//! parsed `docker image ls` rows (`cleanup::tests`); here every test image is
//! shown to be needed or not as it should be, and never selected.

use std::collections::HashSet;
use std::time::Instant;

use aincient_core::cleanup::{self, Scope};
use aincient_core::engine::{pinned_tag, protect_image, IMAGE_ENV};
use aincient_core::ops;
use aincient_core::sites::{Registry, Site as RegisteredSite};
use aincient_core::stack::IMAGE_REPO;

use crate::harness::{docker, try_docker, user_home, Feed, Site, Template, SCN_DB_IMAGE};

/// The registry this scenario writes into the run's user home; removed after.
struct RegistryGuard(std::path::PathBuf);

impl Drop for RegistryGuard {
    fn drop(&mut self) {
        let dir = self.0.join(".atelier");
        if dir.starts_with(std::env::temp_dir()) {
            let _ = std::fs::remove_dir_all(dir);
        }
    }
}

fn registered(site: &Site, slug: &str) -> RegisteredSite {
    RegisteredSite {
        slug: slug.to_string(),
        label: slug.to_string(),
        home: site.stack.home.clone(),
        project: site.project().to_string(),
        remote: None,
    }
}

/// `docker image ls` rows of `id`: `(repo, tag, digest)`.
fn rows(id: &str) -> Vec<(String, String, String)> {
    docker(&[
        "image",
        "ls",
        "--no-trunc",
        "--digests",
        "--format",
        "{{.ID}}\t{{.Repository}}\t{{.Tag}}\t{{.Digest}}",
    ])
    .lines()
    .filter_map(|l| {
        let f: Vec<&str> = l.split('\t').collect();
        (f.first() == Some(&id)).then(|| (f[1].into(), f[2].into(), f[3].into()))
    })
    .collect()
}

/// Whether `needed` covers image `id` the way clean-up compares: by ID, by a
/// `repo:tag` it is listed under, or by `repo@digest`.
fn covered(needed: &HashSet<String>, id: &str) -> bool {
    needed.contains(id)
        || rows(id).iter().any(|(repo, tag, digest)| {
            (tag != "<none>" && needed.contains(&format!("{repo}:{tag}")))
                || (digest != "<none>" && needed.contains(&format!("{repo}@{digest}")))
        })
}

fn image_id(image: &str) -> String {
    docker(&["image", "inspect", "-f", "{{.Id}}", image])
}

#[test]
#[ignore = "real Docker: cargo test -p aincient-core --test scenarios -- --ignored --test-threads=1"]
fn clean_up_counts_every_image_a_registered_site_needs() {
    let t = Instant::now();
    let home = user_home();
    let _registry = RegistryGuard(home.clone());

    // A stopped site: its containers are there, its image used by none running.
    let mut stopped = Site::new("cleanup-stopped");
    let (stopped_image, stopped_id) = stopped.build_app("app", "s");
    stopped.scaffold(&stopped_image, Template::Current);
    stopped.up_ready();
    ops::stop(&stopped.stack, &mut Feed::default()).expect("stop");
    // Its image as it was before its last update: superseded, dangling now
    // (the classic image store keeps it untagged; the containerd store
    // deletes a build that loses its last name, so there it has no rows).
    let (_, superseded) = stopped.build_app("old", "s0");
    stopped.build_app("old", "s1");
    assert!(rows(&superseded)
        .iter()
        .all(|(r, t, _)| r == "<none>" && t == "<none>"));

    // A site pinned by digest (an image as the registry named it).
    let digest_ref = docker(&[
        "image",
        "inspect",
        "-f",
        "{{index .RepoDigests 0}}",
        SCN_DB_IMAGE,
    ]);
    assert!(digest_ref.contains("@sha256:"), "{digest_ref}");
    let pinned = Site::new("cleanup-digest");
    pinned.scaffold(&digest_ref, Template::Current);

    // An engine site: its engine pinned by image ID and kept by the real
    // `protect_image` under `pinned-<project>`, its build tag gone (an image
    // pulled by digest alone has none).
    let mut engine = Site::new("cleanup-engine");
    let (engine_build, engine_id) = engine.build_app("engine", "e");
    engine.scaffold(&engine_build, Template::Current);
    engine.set_env(IMAGE_ENV, &engine_id);
    protect_image(&engine.stack, &engine_id);
    let pin = pinned_tag(engine.project());
    engine.owns_tag(&pin);
    assert_eq!(image_id(&pin), engine_id, "the real pin tagged the engine");
    docker(&["image", "rm", &engine_build]);

    // An unrelated generic image, used by nothing: tagged by name (an image
    // ID is no name to tag by on the containerd store once its build lost
    // every name).
    let (generic_build, _) = engine.build_app("generic", "g");
    let generic = format!("{}-generic:1", engine.project());
    docker(&["tag", &generic_build, &generic]);
    engine.owns_tag(&generic);

    // A build made on this computer under Atelier's own repository (as a
    // developer or fork build is), used by nothing and needed by no site.
    // "Everything" takes unused Atelier images, but only ones that can be
    // downloaded again: this one can't. On the containerd image store it lists
    // a registry digest like a pulled image (Linux CI, Docker 29), so a digest
    // is no proof there.
    let (atelier_build, atelier_local) = engine.build_app("atelier", "l");
    let atelier_name = format!("{IMAGE_REPO}:{}", engine.project());
    docker(&["tag", &atelier_build, &atelier_name]);
    engine.owns_tag(&atelier_name);
    docker(&["image", "rm", &atelier_build]);

    Registry {
        active: None,
        sites: vec![
            registered(&stopped, "scn-stopped"),
            registered(&pinned, "scn-digest"),
            registered(&engine, "scn-engine"),
        ],
    }
    .save(&home)
    .expect("the scenario's registry");

    let needed = cleanup::needed_images(&home).expect("the needed set");
    assert!(covered(&needed, &stopped_id), "the stopped site's image");
    assert!(
        needed.contains(&digest_ref),
        "the digest pin, by repo@digest: {needed:?}"
    );
    assert!(covered(&needed, &image_id(&digest_ref)));
    assert!(needed.contains(&pin), "the engine pin's local name");
    assert!(covered(&needed, &engine_id), "the engine image");
    for base in [SCN_DB_IMAGE, "nginx:1.30-alpine-slim"] {
        assert!(
            covered(&needed, &image_id(base)),
            "{base}, named in compose.yaml"
        );
    }
    assert!(
        !covered(&needed, &superseded),
        "the superseded build isn't needed"
    );
    assert!(!needed.contains(&generic), "nor the generic image");

    // Selection only (never `free`): neither pass picks any of them.
    let ours: Vec<String> = [&stopped_id, &engine_id, &superseded, &atelier_local]
        .into_iter()
        .cloned()
        .chain([image_id(&digest_ref)])
        .collect();
    for scope in [Scope::AfterUpdate, Scope::Everything] {
        let found = cleanup::find(scope).expect("what clean-up would remove");
        let picked: Vec<&String> = found
            .images
            .iter()
            .map(|i| &i.id)
            .filter(|id| ours.contains(id))
            .collect();
        assert!(
            picked.is_empty(),
            "{scope:?} picked a scenario image: {picked:?}"
        );
    }
    assert!(try_docker(&["image", "inspect", &generic]).is_ok());
    eprintln!("scenario 7 took {:.0?}", t.elapsed());
}
