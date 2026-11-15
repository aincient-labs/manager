//! Scenario 26 — update of a site on a build made on this computer (state
//! zoo, 2026-11-15: `zoo-shared-tag-a` on `atelier-cms:zoo-shared`): no
//! registry has the image, so the update ran `compose pull` and failed with
//! Docker's "pull access denied for atelier-cms", exit 1, no way forward. It
//! must refuse before anything is touched ([`LocalBuild`]), say there is
//! nothing to download and how to follow released versions again, and change
//! nothing: `.env`, `compose.yaml`, the containers, the image, no backup.
//!
//! The harness's `atelier-scn-app:*` tags are such builds. Unlike scenarios 4
//! and 6, this site has no [`Site::local_images`] overlay: that one sets
//! `pull_policy: never` on `app`, which tells compose to use the local image as
//! it is, so those updates still go ahead (the detection lets that through).
//! The tag is moved to a second build first, as the shared tag moves: the
//! refused update must not move the site to it either.

use std::time::Instant;

use aincient_core::ops::{self, LocalBuild, Overridable, UpdateOptions};

use crate::harness::{Feed, Site, Template};

#[test]
#[ignore = "real Docker: cargo test -p aincient-core --test scenarios -- --ignored --test-threads=1"]
fn update_of_a_site_on_a_local_build_is_refused_and_changes_nothing() {
    let t = Instant::now();
    let mut site = Site::new("local-build");
    let (app, old) = site.build_app("app", "a");
    site.scaffold(&app, Template::Current);
    site.up_ready();
    let (_, new) = site.build_app("app", "b");
    assert_ne!(old, new);

    let env = std::fs::read_to_string(site.stack.env_path()).expect(".env");
    let compose = std::fs::read_to_string(site.stack.compose_path()).expect("compose.yaml");
    let ids: Vec<String> = ["db", "app", "edge"].iter().map(|s| site.id(s)).collect();

    // The Home update line: no update is offered for such a site.
    let check = ops::check_update(&site.stack);
    assert_ne!(
        check.update_available,
        Some(true),
        "no update offered: {check:?}"
    );

    let mut feed = Feed::default();
    let refused = ops::update_with(&site.stack, &UpdateOptions::default(), &mut feed)
        .expect_err("an update with nothing to download must stop");
    assert!(LocalBuild::is(&refused), "the typed refusal: {refused:#}");
    assert_eq!(Overridable::of(&refused), None, "nothing to override");
    let said = format!("{refused:#}");
    assert!(
        said.contains(&format!("a build made on this computer ({app})"))
            && said.contains("nothing to download")
            && said.contains("`atelier app channel stable`"),
        "the plain refusal with its way forward: {said}"
    );
    assert!(
        !said.contains("pull access denied"),
        "no raw Docker text: {said}"
    );
    assert!(
        !feed.lines.iter().any(|l| l.contains("Pulling")),
        "nothing was pulled: {:?}",
        feed.lines
    );

    assert_eq!(
        std::fs::read_to_string(site.stack.env_path()).expect(".env"),
        env,
        ".env unchanged"
    );
    assert_eq!(
        std::fs::read_to_string(site.stack.compose_path()).expect("compose.yaml"),
        compose,
        "compose.yaml unchanged"
    );
    let after: Vec<String> = ["db", "app", "edge"].iter().map(|s| site.id(s)).collect();
    assert_eq!(after, ids, "the same containers");
    assert_eq!(site.inspect("app", "{{.Image}}"), old, "still its build");
    for s in ["db", "app", "edge"] {
        assert!(site.running(s), "{s} still runs");
    }
    assert!(ops::list_backups(&site.stack).is_empty(), "no backup taken");
    eprintln!("scenario 26 took {:.0?}", t.elapsed());
}
