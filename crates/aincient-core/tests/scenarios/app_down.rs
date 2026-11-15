//! Scenario 25 — removing a site's containers (`atelier app down`, the app's
//! Remove) and erasing it (`--wipe`, Remove and erase; the marker since
//! a7af01f, which also brought the erased-database refusal it exempts):
//!
//! - without wipe, `compose down` takes the containers and keeps the volumes:
//!   the next Start creates the containers again (`up -d --no-recreate`) on the
//!   same data.
//! - with wipe, the owner's-wipe marker is written first, then `down -v` takes
//!   the volumes: the next Start sets up a fresh site instead of refusing with
//!   `DatabaseErased` (the site has a `site-id`, so a database volume gone
//!   without the marker is refused, scenario 8), and clears the marker.

use std::time::Instant;

use aincient_core::ops;

use crate::harness::{docker, Feed, Site, Template};

#[test]
#[ignore = "real Docker: cargo test -p aincient-core --test scenarios -- --ignored --test-threads=1"]
fn app_down_keeps_the_data_and_down_wipe_starts_a_fresh_site() {
    let t = Instant::now();
    let mut site = Site::new("app-down");
    let (app, _) = site.build_app("app", "a");
    site.scaffold(&app, Template::Current);
    site.up_ready();
    site.sql("CREATE TABLE scn_marker (v text); INSERT INTO scn_marker VALUES ('kept')");
    assert!(site.stack.site_id().is_some(), "a site with an id");
    let volumes = |site: &Site| {
        docker(&[
            "volume",
            "ls",
            "-q",
            "--filter",
            &format!("label=com.docker.compose.project={}", site.project()),
        ])
    };
    let db_volume = format!("{}_db-data", site.project());
    assert!(volumes(&site).lines().any(|v| v == db_volume));

    // Remove (keep data).
    ops::down(&site.stack, false, &mut Feed::default()).expect("down");
    assert_eq!(site.compose(&["ps", "-a", "-q"]), "", "containers removed");
    assert!(
        volumes(&site).lines().any(|v| v == db_volume),
        "the data volumes stay"
    );
    assert!(!site.stack.database_wiped(), "no wipe marker");
    let ready = ops::start(&site.stack, &mut Feed::default());
    assert!(
        matches!(ready, Ok(true)),
        "Start creates the containers again: {ready:?}"
    );
    assert_eq!(
        site.sql("SELECT v FROM scn_marker"),
        "kept",
        "on the same data"
    );

    // Remove and erase.
    ops::down(&site.stack, true, &mut Feed::default()).expect("down --wipe");
    assert_eq!(site.compose(&["ps", "-a", "-q"]), "", "containers removed");
    assert_eq!(volumes(&site), "", "every volume erased");
    assert!(
        site.stack.database_wiped_path().is_file() && site.stack.database_wiped(),
        "the owner's-wipe marker is there"
    );
    let ready = ops::start(&site.stack, &mut Feed::default());
    assert!(
        matches!(ready, Ok(true)),
        "an erased site starts fresh, not refused: {ready:?}"
    );
    assert_eq!(
        site.sql("SELECT coalesce(to_regclass('scn_marker')::text, 'none')"),
        "none",
        "a fresh, empty database"
    );
    assert!(
        !site.stack.database_wiped_path().exists(),
        "the marker is cleared once the site started again"
    );
    assert!(volumes(&site).lines().any(|v| v == db_volume));
    eprintln!("scenario 25 took {:.0?}", t.elapsed());
}
