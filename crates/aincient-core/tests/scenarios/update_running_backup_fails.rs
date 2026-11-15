//! Scenario 6b — update of a RUNNING site whose before-update backup fails
//! (prior-art review, 2026-11-14): the backup failed and the update logged
//! "Couldn't take a backup first … Continuing." and migrated anyway, with no
//! way back — what a full disk does. It must stop with
//! [`BackupBeforeUpdateFailed`] and leave the site on its build; told to go
//! without the backup (`--no-backup`), it updates.
//!
//! The backup fails because the backups folder is read-only (as scenario 5);
//! the site's tag is moved to a new local build (the "download"), read
//! through [`Site::local_images`] as in scenario 4.
//!
//! The folder is tried before the dump, so the refusal says plainly that
//! Atelier can't save backups there; stopped, the site isn't even started
//! for the backup (state zoo, `ro-backups`).

use std::time::Instant;

use aincient_core::ops::{self, BackupBeforeUpdateFailed, Overridable, UpdateOptions, Updated};

use crate::harness::{assert_cleanup_removes_nothing, set_mode, Feed, Site, Template};

#[test]
#[ignore = "real Docker: cargo test -p aincient-core --test scenarios -- --ignored --test-threads=1"]
fn update_of_a_running_site_whose_backup_fails_is_refused_then_proceeds_without_a_backup() {
    let t = Instant::now();
    let mut site = Site::new("update-backup-fails");
    let (app, old) = site.build_app("app", "a");
    site.scaffold(&app, Template::Current);
    site.local_images();
    site.up_ready();
    // The download: the site's tag now names a new build.
    let (_, new) = site.build_app("app", "b");
    assert_ne!(old, new);

    let backups = site.stack.backups_dir();
    std::fs::create_dir_all(&backups).expect("backups dir");
    set_mode(&backups, 0o555);
    assert!(
        std::fs::write(backups.join("probe"), "").is_err(),
        "the backups folder must be unwritable for this scenario (running as root?)"
    );

    let refused = ops::update_with(&site.stack, &UpdateOptions::default(), &mut Feed::default())
        .expect_err("an update whose backup failed must stop");
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
        said.contains("Couldn't back up your site first: Atelier can't save backups in ")
            && said.contains("it isn't writable")
            && !said.contains("permission denied"),
        "the plain cause, found before any dump: {said}"
    );
    assert_eq!(
        site.inspect("app", "{{.Image}}"),
        old,
        "not updated: still the build it ran"
    );
    assert!(site.running("app"), "the site keeps running");

    // Stopped, the site would be started only to be backed up: refused
    // before it is started, with the same cause.
    ops::stop(&site.stack, &mut Feed::default()).expect("stop");
    let refused = ops::update_with(&site.stack, &UpdateOptions::default(), &mut Feed::default())
        .expect_err("a stopped site whose backup can't be saved isn't started for it");
    assert!(
        format!("{refused:#}").contains("Atelier can't save backups in "),
        "{refused:#}"
    );
    assert_eq!(
        Overridable::of(&refused),
        Some(Overridable::UpdateWithoutBackup)
    );
    assert!(
        !site.running("app"),
        "not started for a backup it can't save"
    );
    assert_eq!(site.inspect("app", "{{.Image}}"), old, "still its build");
    ops::start(&site.stack, &mut Feed::default()).expect("start again");

    set_mode(&backups, 0o755);
    assert_cleanup_removes_nothing();
    let done = ops::update_with(
        &site.stack,
        &UpdateOptions { no_backup: true },
        &mut Feed::default(),
    );
    assert!(
        matches!(done, Ok(Updated::Ready)),
        "the update proceeds without a backup: {done:?}"
    );
    assert_eq!(site.inspect("app", "{{.Image}}"), new, "on the new build");
    assert!(
        ops::list_backups(&site.stack).is_empty(),
        "no backup was taken"
    );
    eprintln!("scenario 6b took {:.0?}", t.elapsed());
}
