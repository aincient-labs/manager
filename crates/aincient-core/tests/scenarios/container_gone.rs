//! Scenario 20 — one container of a stopped site is gone (`docker rm`, a
//! Docker clean-up; fixed in a7af01f, `--no-recreate` since 4c1b422): before,
//! Start saw the `app` container, ran `compose start`, and started app and edge
//! against no database. Start must see the missing one, create only it
//! (`up -d --no-recreate`): the others keep their containers, and the app keeps
//! the image it had even though the site's shared tag has moved on since (a
//! plain `up` would recreate it on the new one: an update without a backup).
//! The database comes back on its volume, with its data.
//!
//! Twice: the db container gone, then edge's.

use std::time::Instant;

use aincient_core::ops;

use crate::harness::{docker, try_docker, Feed, Site, Template};

#[test]
#[ignore = "real Docker: cargo test -p aincient-core --test scenarios -- --ignored --test-threads=1"]
fn start_recreates_only_the_removed_container_and_keeps_the_others() {
    let t = Instant::now();
    let mut site = Site::new("container-gone");
    let (image, first) = site.build_app("shared", "a");
    site.scaffold(&image, Template::Current);
    site.up_ready();
    site.sql("CREATE TABLE scn_marker (v text); INSERT INTO scn_marker VALUES ('kept')");
    // Another site's update moves the shared tag on.
    let (_, second) = site.build_app("shared", "b");
    assert_ne!(first, second);

    for gone in ["db", "edge"] {
        ops::stop(&site.stack, &mut Feed::default()).expect("stop");
        let ids = |site: &Site| -> Vec<(&str, String)> {
            ["db", "app", "edge"]
                .into_iter()
                .filter(|s| *s != gone)
                .map(|s| (s, site.id(s)))
                .collect()
        };
        let kept = ids(&site);
        let removed = site.id(gone);
        docker(&["rm", "-f", &removed]);
        assert_eq!(
            site.compose(&["ps", "-a", "-q", gone]),
            "",
            "{gone} has no container"
        );

        let mut feed = Feed::default();
        let ready = ops::start(&site.stack, &mut feed);
        assert!(
            matches!(ready, Ok(true)),
            "Start must bring the site up without {gone}'s container: {ready:?}"
        );
        assert!(
            try_docker(&["inspect", &removed]).is_err(),
            "the removed container stays gone"
        );
        assert_ne!(site.id(gone), removed, "{gone} was created again");
        assert_eq!(ids(&site), kept, "the other containers were kept");
        for s in ["db", "app", "edge"] {
            assert!(site.running(s), "{s} runs");
        }
        assert_eq!(
            site.inspect("app", "{{.Image}}"),
            first,
            "the app keeps its image"
        );
        assert!(site.app_reaches_db(), "app reaches `db` by name");
        assert_eq!(site.sql("SELECT v FROM scn_marker"), "kept", "data kept");
    }
    eprintln!("scenario 20 took {:.0?}", t.elapsed());
}
