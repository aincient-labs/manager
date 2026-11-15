//! Scenario 8 — the database volume erased (S3 in a7af01f, doctor in afdbc1e):
//! a site that has been backed up (it has a `site-id`) loses its db volume to
//! a Docker reset or a prune. `compose up` would make an empty one and the
//! appliance would install a blank site over the lost one. Start and the
//! checkup's StartContainers repair must refuse with the backup to restore,
//! and create nothing; a restore brings the site back with its data.
//!
//! Real `ops::start`, `doctor::fix`, `ops::backup` and `ops::restore_with`
//! (the restore's in-container scripts run the app's scenario `drush`, as in
//! scenario 5).

use std::time::Instant;

use aincient_core::doctor::{self, Repair};
use aincient_core::ops::{self, DatabaseErased, RestoreOptions};

use crate::harness::{docker, try_docker, Feed, Site, Template};

#[test]
#[ignore = "real Docker: cargo test -p aincient-core --test scenarios -- --ignored --test-threads=1"]
fn start_and_doctor_refuse_an_erased_database_and_a_restore_brings_it_back() {
    let t = Instant::now();
    let mut site = Site::new("erased-db");
    let (app, _) = site.build_app("app", "a");
    site.scaffold(&app, Template::Current);
    site.up_ready();
    site.sql("CREATE TABLE scn_marker (v text); INSERT INTO scn_marker VALUES ('the site')");
    let file = ops::backup(&site.stack, None, &mut Feed::default()).expect("a real backup");
    let name = file.file_name().unwrap().to_string_lossy().into_owned();
    assert!(
        site.stack.site_id().is_some(),
        "a backed-up site has its id"
    );

    // A Docker reset: the containers and the database volume are gone.
    site.compose(&["down", "-t", "2"]);
    let volume = format!("{}_db-data", site.project());
    docker(&["volume", "rm", &volume]);
    let nothing_made = |site: &Site, step: &str| {
        assert_eq!(
            site.compose(&["ps", "-a", "-q"]),
            "",
            "{step} created no container"
        );
        assert!(
            try_docker(&["volume", "inspect", &volume]).is_err(),
            "{step} created no empty database volume"
        );
    };

    let refused = ops::start(&site.stack, &mut Feed::default())
        .expect_err("Start must refuse an erased database");
    let erased = refused
        .downcast_ref::<DatabaseErased>()
        .unwrap_or_else(|| panic!("a DatabaseErased: {refused:#}"));
    assert_eq!(
        erased.latest_backup.as_ref().map(|b| b.name.as_str()),
        Some(name.as_str()),
        "it names the backup to restore"
    );
    assert!(
        refused.to_string().contains("Restore your latest backup"),
        "{refused:#}"
    );
    nothing_made(&site, "Start");

    // The checkup's repair ladder: Start the containers, refused the same way.
    let report = doctor::fix(&site.stack, &mut Feed::default()).expect("not busy");
    let start = report
        .actions
        .iter()
        .find(|a| a.repair == Repair::StartContainers)
        .unwrap_or_else(|| panic!("doctor tried to start the site: {:?}", report.actions));
    assert!(!start.succeeded, "{start:?}");
    let detail = start.detail.as_deref().unwrap_or_default();
    assert!(
        detail.contains("database was removed") && detail.contains(&name),
        "the repair says why and names the backup: {detail}"
    );
    nothing_made(&site, "doctor --fix");

    // The way back: restore that backup.
    ops::restore_with(
        &site.stack,
        &file,
        &RestoreOptions::default(),
        &mut Feed::default(),
    )
    .expect("restore into the erased site");
    assert!(site.running("app") && site.running("db"));
    assert_eq!(site.sql("SELECT v FROM scn_marker"), "the site");
    eprintln!("scenario 8 took {:.0?}", t.elapsed());
}
