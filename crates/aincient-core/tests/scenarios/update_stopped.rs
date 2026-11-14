//! Scenario 6 — update of a stopped site that won't boot (B3, fixed in
//! 11dd0cc): an update backs the site up first, so a stopped site is started
//! for its backup. When it won't start (its build fails its converge on this
//! data), the update must stop with [`BackupBeforeUpdateFailed`] carrying the
//! plain reason, and leave the site on its version. Told to go without the
//! backup (`--no-backup`), the update proceeds and the new build boots.
//!
//! The site's tag is moved to a new local build (the "download"), read through
//! [`Site::local_images`] as in scenario 4. Build `a` fails its converge once
//! the flag is in the `private` volume ([`Site::fail_boot`]); build `b` doesn't.

use std::time::Instant;

use aincient_core::ops::{self, BackupBeforeUpdateFailed, Overridable, UpdateOptions, Updated};

use crate::harness::{assert_cleanup_removes_nothing, Feed, Site, Template};

#[test]
#[ignore = "real Docker: cargo test -p aincient-core --test scenarios -- --ignored --test-threads=1"]
fn update_of_a_stopped_site_that_wont_boot_is_refused_then_proceeds_without_a_backup() {
    let t = Instant::now();
    let mut site = Site::new("update-stopped");
    let (app, old) = site.build_app("app", "a");
    site.scaffold(&app, Template::Current);
    site.local_images();
    site.up_ready();
    site.fail_boot("a");
    ops::stop(&site.stack, &mut Feed::default()).expect("stop");
    // The download: the site's tag now names a new build.
    let (_, new) = site.build_app("app", "b");
    assert_ne!(old, new);

    let mut feed = Feed::default();
    let refused = ops::update_with(&site.stack, &UpdateOptions::default(), &mut feed)
        .expect_err("an update that can't back up a stopped site first must stop");
    assert!(
        refused.downcast_ref::<BackupBeforeUpdateFailed>().is_some(),
        "the typed refusal: {refused:#}"
    );
    assert_eq!(
        Overridable::of(&refused),
        Some(Overridable::UpdateWithoutBackup)
    );
    let said = format!("{refused:#}");
    assert!(
        said.contains("couldn't be started to back it up first")
            && said.contains("post-update health check failed"),
        "the cause, in plain words: {said}"
    );
    assert_eq!(
        site.inspect("app", "{{.Image}}"),
        old,
        "not updated: still the build it ran"
    );
    assert!(ops::list_backups(&site.stack).is_empty());

    // The owner's override. It ends Ready, which runs the automatic clean-up.
    assert_cleanup_removes_nothing();
    let opts = UpdateOptions { no_backup: true };
    let done = ops::update_with(&site.stack, &opts, &mut Feed::default());
    assert!(
        matches!(done, Ok(Updated::Ready)),
        "the update proceeds without a backup: {done:?}"
    );
    assert_eq!(site.inspect("app", "{{.Image}}"), new, "on the new build");
    assert!(site.running("app") && site.running("db") && site.running("edge"));
    assert!(
        ops::list_backups(&site.stack).is_empty(),
        "no backup was taken"
    );
    eprintln!("scenario 6 took {:.0?}", t.elapsed());
}
