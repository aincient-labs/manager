//! Scenario 21 — the checkup's StartContainers repair on a site with a
//! container gone and one stopped (`--no-recreate` since 4c1b422): `doctor
//! --fix` ran `compose up -d`, which also recreates every container whose image
//! the shared tag no longer names, so a repair moved the app to another site's
//! newer build with no backup. It must create only the missing container,
//! start the stopped one in place, and leave the site Ready on its own image.
//!
//! compose.yaml is exactly the shipped template (the scenario db image comes
//! from an overlay), so the checkup's first rung, the Scaffold rewrite, has
//! nothing to do and StartContainers is the repair that runs.

use std::time::Instant;

use aincient_core::doctor::{self, Repair};

use crate::harness::{docker, try_docker, Feed, Site, Template};

#[test]
#[ignore = "real Docker: cargo test -p aincient-core --test scenarios -- --ignored --test-threads=1"]
fn doctor_fix_creates_the_removed_container_and_keeps_the_sites_image() {
    let t = Instant::now();
    let mut site = Site::new("doctor-start");
    let (image, first) = site.build_app("shared", "a");
    site.scaffold(&image, Template::Current);
    site.db_image_by_overlay();
    site.up_ready();
    site.sql("CREATE TABLE scn_marker (v text); INSERT INTO scn_marker VALUES ('kept')");
    // Another site's update moves the shared tag on.
    let (_, second) = site.build_app("shared", "b");
    assert_ne!(first, second);

    // edge removed (a Docker clean-up), app stopped.
    let edge = site.id("edge");
    docker(&["rm", "-f", &edge]);
    docker(&["stop", "-t", "2", &site.container("app")]);
    let db_id = site.id("db");
    let app_id = site.id("app");

    let before = doctor::diagnose(&site.stack);
    assert!(
        before.available_repairs().first() == Some(&Repair::StartContainers),
        "the checkup offers to start the containers first: {:?}",
        before.available_repairs()
    );

    let report = doctor::fix(&site.stack, &mut Feed::default()).expect("not busy");
    let start = report
        .actions
        .iter()
        .find(|a| a.repair == Repair::StartContainers)
        .unwrap_or_else(|| panic!("doctor started the site: {:?}", report.actions));
    assert!(start.succeeded, "{start:?}");
    assert_eq!(
        start.detail, None,
        "Ready before the checkup looked again: {start:?}"
    );
    assert!(
        !report.actions.iter().any(|a| a.repair == Repair::Scaffold),
        "no compose.yaml rewrite: {:?}",
        report.actions
    );

    assert!(try_docker(&["inspect", &edge]).is_err(), "old edge gone");
    assert_ne!(site.id("edge"), edge, "edge was created again");
    assert_eq!(site.id("db"), db_id, "db kept its container");
    assert_eq!(
        site.id("app"),
        app_id,
        "app started in place, not recreated"
    );
    assert_eq!(
        site.inspect("app", "{{.Image}}"),
        first,
        "the repair must not move the site to the new image"
    );
    for s in ["db", "app", "edge"] {
        assert!(site.running(s), "{s} runs");
    }
    let stack_checks: Vec<_> = report
        .checks
        .iter()
        .filter(|c| c.id.starts_with("stack.") && c.severity == doctor::Severity::Fail)
        .collect();
    assert!(
        stack_checks.is_empty(),
        "no stack check fails after: {stack_checks:?}"
    );
    assert!(site.app_reaches_db(), "app reaches `db` by name");
    assert_eq!(site.sql("SELECT v FROM scn_marker"), "kept");
    eprintln!("scenario 21 took {:.0?}", t.elapsed());
}
