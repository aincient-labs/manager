//! Scenario 23 — the database server move (85f213b; download first since
//! 2133546): a site on the legacy db image, once it runs an app whose converge
//! rebuilds indexes, is moved by an update onto the current db image: `docker
//! pull` of the new image, then `compose.yaml` rewritten to it, then `compose up
//! -d --force-recreate db app`, keeping the data. When the download fails (no
//! network, a registry hiccup), the update must still end Ready on the old db
//! image with `compose.yaml` untouched, and the next update moves it. Before
//! 2133546 the file was rewritten first and the failed download then failed the
//! update, leaving a site whose compose.yaml named an image it didn't have.
//!
//! The seam: the real pair is `pgvector/pgvector:pg16` → `…:pg16-trixie`,
//! which a scenario may not pull or tag. With the `scenario-seams` feature
//! (on for this crate's tests only), `stack::scenario_seams::stand_in_db_move`
//! adds a pair of scenario images to the one `db_image_upgrade` knows; the
//! rest is the product's own update. Both are scenario Postgres builds; the new
//! one is served by a registry on this computer (`harness::Registry`), so the
//! Manager's `docker pull` is a real download. Phase 1 runs before it is
//! pushed (the download fails), phase 2 after.

use std::time::Instant;

use aincient_core::ops::{self, UpdateOptions, Updated};
use aincient_core::stack::scenario_seams::stand_in_db_move;

use crate::harness::{
    assert_cleanup_removes_nothing, docker, try_docker, Feed, Registry, Site, Template,
};

#[test]
#[ignore = "real Docker: cargo test -p aincient-core --test scenarios -- --ignored --test-threads=1"]
fn update_moves_the_db_image_once_it_downloads_and_keeps_the_data() {
    let t = Instant::now();
    // Declared first, so it outlives the site.
    let registry = Registry::start();
    let mut site = Site::new("db-move");
    let legacy = format!("{}-db:legacy", site.project());
    let current = format!("{}/{}-db:current", registry.host, site.project());
    let legacy_id = site.build_db(&legacy, "legacy");
    let _move = stand_in_db_move(&legacy, &current);

    let (app, _) = site.build_app("app", "a");
    site.scaffold(&app, Template::Current);
    site.set_db_image(&legacy);
    site.local_images();
    site.up_ready();
    site.sql("CREATE TABLE scn_marker (v text); INSERT INTO scn_marker VALUES ('kept')");
    assert_eq!(site.inspect("db", "{{.Image}}"), legacy_id);
    assert_eq!(
        site.stack.pending_db_image_migration(),
        Some((legacy.clone(), current.clone()))
    );
    let compose_names = |site: &Site, image: &str| {
        std::fs::read_to_string(site.stack.compose_path())
            .unwrap()
            .lines()
            .any(|l| l.trim() == format!("image: {image}"))
    };

    // The release whose converge rebuilds indexes (the move's precondition).
    let (_, new_app) = site.build_app_converging("app", "b");

    // Phase 1: the new db image can't be downloaded yet.
    assert!(try_docker(&["image", "inspect", &current]).is_err());
    assert_cleanup_removes_nothing();
    let mut feed = Feed::default();
    let done = ops::update_with(&site.stack, &UpdateOptions::default(), &mut feed);
    assert!(
        matches!(done, Ok(Updated::Ready)),
        "a failed db download doesn't fail the update: {done:?}"
    );
    assert_eq!(site.inspect("app", "{{.Image}}"), new_app, "app updated");
    assert!(
        feed.lines
            .iter()
            .any(|l| l.starts_with("Warning: couldn't download the database server")),
        "it says the move waits: {:?}",
        feed.lines
    );
    assert!(compose_names(&site, &legacy), "compose.yaml untouched");
    assert!(!compose_names(&site, &current));
    assert_eq!(site.inspect("db", "{{.Image}}"), legacy_id, "db not moved");
    assert_eq!(site.sql("SELECT v FROM scn_marker"), "kept");

    // Phase 2: the registry has it now (and this computer doesn't).
    let current_id = site.build_db(&current, "current");
    assert_ne!(current_id, legacy_id);
    registry.push(&current);
    docker(&["image", "rm", &current]);
    let db_before = site.id("db");

    assert_cleanup_removes_nothing();
    let mut feed = Feed::default();
    let done = ops::update_with(&site.stack, &UpdateOptions::default(), &mut feed);
    assert!(
        matches!(done, Ok(Updated::Ready)),
        "the next update moves the db: {done:?}"
    );
    assert!(
        feed.lines
            .iter()
            .any(|l| l.contains("Moving your site's database server to")),
        "{:?}",
        feed.lines
    );
    assert!(
        compose_names(&site, &current),
        "compose.yaml names the new db image"
    );
    assert!(!compose_names(&site, &legacy));
    assert_eq!(site.stack.pending_db_image_migration(), None);
    assert_eq!(
        site.inspect("db", "{{.Image}}"),
        current_id,
        "db runs the downloaded image"
    );
    assert_ne!(site.id("db"), db_before, "db was recreated");
    assert_eq!(site.inspect("app", "{{.Image}}"), new_app);
    for s in ["db", "app", "edge"] {
        assert!(site.running(s), "{s} runs");
    }
    assert!(site.app_reaches_db(), "app reaches `db` by name");
    assert_eq!(
        site.sql("SELECT v FROM scn_marker"),
        "kept",
        "data kept across the move"
    );
    eprintln!("scenario 23 took {:.0?}", t.elapsed());
}
