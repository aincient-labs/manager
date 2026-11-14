//! Scenarios 14, 15, 16 — an update refuses while the site isn't ready (M2c,
//! `studies/manager-prior-art.md` verdict 2, the Home Assistant model): it
//! refuses with [`UpdateBlocked`] before anything is downloaded, backed up or
//! changed, and never repairs mid-update. Each site's tag is moved to a new
//! local build first (the "download", read through [`Site::local_images`] as
//! in scenario 4), so an update that wasn't refused would really change it.
//!
//! - 14: db lost its network (scenario 2's state). Before the gate the update
//!   "repaired" it on the way (its Start reconnected db) and updated. Now it
//!   is refused naming the container; after Start reconnects it, it updates.
//! - 15: the app is crash-looping out of memory (scenario 11's state). Before,
//!   the update tried to start it for its backup and failed on the way.
//! - 16: Docker's disk is nearly full, as the probe reads it in the running
//!   database: here its data volume is a 96 MB tmpfs. Before, the update ran.

use std::time::Instant;

use aincient_core::boot::BootFailed;
use aincient_core::doctor::{self, Severity};
use aincient_core::gate::{Blocker, UpdateBlocked, WayForward, NOT_READY};
use aincient_core::ops::{self, UpdateOptions, Updated};

use crate::harness::{assert_cleanup_removes_nothing, docker, Feed, Site, Template};

/// The update must refuse with [`UpdateBlocked`] and leave the site as it
/// was: the app on `old`, no backup, nothing pulled or backed up in the feed.
fn refused_untouched(site: &Site, old: &str) -> Vec<Blocker> {
    let mut feed = Feed::default();
    let refused = ops::update_with(&site.stack, &UpdateOptions::default(), &mut feed)
        .expect_err("an update of a site that isn't ready must be refused");
    let blocked = refused
        .downcast_ref::<UpdateBlocked>()
        .unwrap_or_else(|| panic!("the typed refusal UpdateBlocked: {refused:#}"))
        .clone();
    assert!(refused.to_string().starts_with(NOT_READY), "{refused:#}");
    eprintln!("refused: {refused}");
    for line in &feed.lines {
        assert!(
            !line.contains("Pulling") && !line.contains("Backing up") && !line.contains("Starting"),
            "nothing is done before the refusal: {line}"
        );
    }
    assert_eq!(site.inspect("app", "{{.Image}}"), old, "not updated");
    assert!(ops::list_backups(&site.stack).is_empty(), "no backup taken");
    blocked.blockers
}

#[test]
#[ignore = "real Docker: cargo test -p aincient-core --test scenarios -- --ignored --test-threads=1"]
fn update_of_a_site_with_a_detached_db_is_refused_until_start_reconnects_it() {
    let t = Instant::now();
    let mut site = Site::new("update-detached");
    let (app, old) = site.build_app("app", "a");
    site.scaffold(&app, Template::Current);
    site.local_images();
    site.up_ready();
    let (_, new) = site.build_app("app", "b");
    assert_ne!(old, new);

    // Scenario 2's state: db running on no network, the app stopped.
    let default = site.network("default");
    ops::stop(&site.stack, &mut Feed::default()).expect("stop");
    let db = site.container("db");
    docker(&["network", "disconnect", "-f", &default, &db]);
    docker(&["start", &db]);
    assert!(site.networks("db").is_empty());

    let blockers = refused_untouched(&site, &old);
    assert_eq!(blockers.len(), 1, "{blockers:?}");
    assert_eq!(blockers[0].way_forward(), Some(WayForward::Start));
    let said = blockers[0].message();
    assert!(
        said.contains(&format!("{db} has no connection to {default}")),
        "names the container and its network: {said}"
    );
    assert!(
        site.networks("db").is_empty(),
        "the refusal repaired nothing: {:?}",
        site.networks("db")
    );

    // The way forward: Start reconnects db; then the update proceeds.
    let started = ops::start(&site.stack, &mut Feed::default());
    assert!(matches!(started, Ok(true)), "Start: {started:?}");
    assert!(site.app_reaches_db());
    assert_cleanup_removes_nothing();
    let done = ops::update_with(&site.stack, &UpdateOptions::default(), &mut Feed::default());
    assert!(
        matches!(done, Ok(Updated::Ready)),
        "then it updates: {done:?}"
    );
    assert_eq!(site.inspect("app", "{{.Image}}"), new, "on the new build");
    assert_eq!(ops::list_backups(&site.stack).len(), 1, "backed up first");
    eprintln!("scenario 14 took {:.0?}", t.elapsed());
}

#[test]
#[ignore = "real Docker: cargo test -p aincient-core --test scenarios -- --ignored --test-threads=1"]
fn update_while_the_app_is_crash_looping_out_of_memory_is_refused_with_the_memory_text() {
    let t = Instant::now();
    let mut site = Site::new("update-oom");
    let (app, old) = site.build_app("app", "a");
    site.scaffold(&app, Template::Current);
    site.local_images();
    // Scenario 11's state: build `a` allocates past the app's limit in
    // "converge", and the shipped restart policy keeps starting it again.
    site.limit_memory("app", "48m");
    site.up_ready();
    site.flag("scn-boot-oom-", "a");
    ops::stop(&site.stack, &mut Feed::default()).expect("stop");
    let failed = ops::start(&site.stack, &mut Feed::default()).expect_err("an OOM boot fails");
    assert!(
        failed
            .downcast_ref::<BootFailed>()
            .is_some_and(BootFailed::out_of_memory),
        "{failed:#}"
    );
    let (_, new) = site.build_app("app", "b");
    assert_ne!(old, new);

    let blockers = refused_untouched(&site, &old);
    let memory = blockers
        .iter()
        .find(|b| matches!(b, Blocker::OutOfMemory { .. }))
        .unwrap_or_else(|| panic!("an out-of-memory blocker: {blockers:?}"));
    let said = memory.message();
    assert!(
        said.starts_with(
            "Your site ran out of memory, so Docker stopped part of it (the app). Give Docker \
             more memory"
        ),
        "{said}"
    );
    assert!(said.ends_with("Update once it runs again."), "{said}");
    assert_eq!(
        memory.way_forward(),
        None,
        "Docker's setting, not the app's"
    );
    eprintln!("scenario 15 took {:.0?}", t.elapsed());
}

/// The database's data volume on a 96 MB tmpfs: Postgres fits (≈40 MB), and
/// `df` in the running db reads well under the 2 GiB floor.
const SMALL_DB_DISK: &str = "volumes:
  db-data:
    driver_opts:
      type: tmpfs
      device: tmpfs
      o: size=96m
";

#[test]
#[ignore = "real Docker: cargo test -p aincient-core --test scenarios -- --ignored --test-threads=1"]
fn update_with_too_little_docker_disk_is_refused_and_the_checkup_fails_it() {
    let t = Instant::now();
    let mut site = Site::new("update-low-disk");
    let (app, old) = site.build_app("app", "a");
    site.scaffold(&app, Template::Current);
    site.local_images();
    site.overlay("scn-small-db-disk.yaml", SMALL_DB_DISK);
    site.up_ready();
    let (_, new) = site.build_app("app", "b");
    assert_ne!(old, new);

    let blockers = refused_untouched(&site, &old);
    assert_eq!(blockers.len(), 1, "{blockers:?}");
    assert_eq!(blockers[0].way_forward(), Some(WayForward::FreeUpSpace));
    let said = blockers[0].message();
    assert!(
        said.starts_with("Docker has 0.")
            && said.contains("GB of disk space free; an update needs at least 2.0 GB"),
        "names Docker's disk, not the computer's: {said}"
    );
    assert!(site.running("app") && site.running("db"), "left running");

    let report = doctor::diagnose(&site.stack);
    let check = report
        .checks
        .iter()
        .find(|c| c.id == "host.docker_disk")
        .expect("the checkup reads Docker's disk");
    assert_eq!(check.severity, Severity::Fail, "{check:?}");
    assert!(
        check
            .remedy
            .as_deref()
            .is_some_and(|r| r.contains("updates are refused below 2.0 GB")),
        "{check:?}"
    );
    eprintln!("scenario 16 took {:.0?}", t.elapsed());
}
