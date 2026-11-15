//! Scenario 3 — a shared tag moved (A1, fixed in 4c1b422): sites share moving
//! tags (`atelier-cms:latest`), so after another site's update the tag names a
//! newer image. A Start that refreshed an older shipped compose.yaml ran
//! `compose up -d`, which recreated this site's app on whatever the tag named
//! by then: an update with no backup and no stepped route. Start must keep the
//! site's image and hand the refreshed file's one new setting (db's restart
//! policy) to the existing db in place.

use std::time::Instant;

use aincient_core::ops;
use aincient_core::stack::{COMPOSE_TEMPLATE, SUPERSEDED_COMPOSE_TEMPLATES};

use crate::harness::{docker, with_scn_db, Feed, Site, Template};

#[test]
#[ignore = "real Docker: cargo test -p aincient-core --test scenarios -- --ignored --test-threads=1"]
fn start_after_the_shared_tag_moved_keeps_the_sites_image() {
    let t = Instant::now();
    let mut site = Site::new("shared-tag");
    // One tag, as `atelier-cms:latest` is shared by every site on it.
    let (image, first) = site.build_app("shared", "a");
    // The last template an older Manager shipped: no restart policy on db.
    let previous = SUPERSEDED_COMPOSE_TEMPLATES
        .last()
        .copied()
        .expect("a superseded template");
    site.scaffold(&image, Template::Shipped(previous));
    site.up_ready();

    let app_image = |site: &Site| site.inspect("app", "{{.Image}}");
    let db_restart = |site: &Site| site.inspect("db", "{{.HostConfig.RestartPolicy.Name}}");
    assert_eq!(app_image(&site), first);
    assert_ne!(
        db_restart(&site),
        "unless-stopped",
        "the old template sets none"
    );
    let app_before = site.inspect("app", "{{.Id}}");

    ops::stop(&site.stack, &mut Feed::default()).expect("stop");
    // Another site's update moves the tag on.
    let (_, second) = site.build_app("shared", "b");
    assert_ne!(first, second);
    assert_eq!(
        docker(&["image", "inspect", "-f", "{{.Id}}", &image]),
        second
    );

    let mut feed = Feed::default();
    let ready = ops::start(&site.stack, &mut feed);
    assert!(
        matches!(ready, Ok(true)),
        "Start must bring the site up: {ready:?}"
    );

    // The start did refresh the file (the path that once ran `up -d`) …
    assert_eq!(
        std::fs::read_to_string(site.stack.compose_path()).unwrap(),
        with_scn_db(COMPOSE_TEMPLATE),
        "Start rewrites an untouched older compose.yaml"
    );
    // … and still runs the image the site had, in the same container.
    assert_eq!(
        app_image(&site),
        first,
        "Start must not move the site to the new image"
    );
    assert_eq!(site.inspect("app", "{{.Id}}"), app_before, "not recreated");
    assert_eq!(db_restart(&site), "unless-stopped");
    eprintln!("scenario 3 took {:.0?}", t.elapsed());
}
