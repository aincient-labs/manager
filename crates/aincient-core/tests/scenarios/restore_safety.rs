//! Scenario 5 — a restore whose safety backup fails (B2, fixed in 11dd0cc): a
//! restore drops the database before loading the backup, so it first backs up
//! the site as it is now. When that backup failed (here: the backups folder is
//! read-only, as on a full or locked-down disk) the restore went ahead anyway,
//! and the work since the chosen backup was gone for good. It must refuse
//! with [`SafetyBackupFailed`] and leave the database as it is; told to go
//! without the copy (`--no-safety-backup`), it restores.
//!
//! Real `ops::backup` / `ops::restore_with` with their real in-container
//! scripts; the app's `drush` does those scripts' four calls on the real
//! Postgres (harness `APP_DRUSH`). Not covered: Drupal's own `sql:dump`
//! options and the converge migrate step a real image runs after a restore.

use std::time::Instant;

use aincient_core::ops::{self, Overridable, RestoreOptions, SafetyBackupFailed};

use crate::harness::{set_mode, writable, Feed, Site, Template};

#[test]
#[ignore = "real Docker: cargo test -p aincient-core --test scenarios -- --ignored --test-threads=1"]
fn restore_refuses_when_its_safety_backup_fails_and_restores_when_told_to_go_without() {
    let t = Instant::now();
    let mut site = Site::new("restore-safety");
    let (app, _) = site.build_app("app", "a");
    site.scaffold(&app, Template::Current);
    site.up_ready();

    site.sql("CREATE TABLE scn_marker (v text); INSERT INTO scn_marker VALUES ('in the backup')");
    let file = ops::backup(&site.stack, None, &mut Feed::default()).expect("a real backup");
    // Work done since that backup: what a restore without a safety copy loses.
    site.sql("UPDATE scn_marker SET v = 'since the backup'");

    let backups = site.stack.backups_dir();
    set_mode(&backups, 0o555);
    assert!(
        std::fs::write(backups.join("probe"), "").is_err(),
        "the backups folder must be unwritable for this scenario (running as root?)"
    );

    let mut feed = Feed::default();
    let refused = ops::restore_with(&site.stack, &file, &RestoreOptions::default(), &mut feed)
        .expect_err("a restore whose safety backup failed must stop");
    assert!(
        refused.is::<SafetyBackupFailed>(),
        "the typed refusal: {refused:#}"
    );
    assert_eq!(
        Overridable::of(&refused),
        Some(Overridable::RestoreWithoutSafetyBackup)
    );
    assert_eq!(
        site.sql("SELECT v FROM scn_marker"),
        "since the backup",
        "the database is untouched"
    );
    assert_eq!(ops::list_backups(&site.stack).len(), 1, "no new file");
    assert!(site.running("app"));

    // The owner's override: restore without the copy.
    let without = RestoreOptions {
        no_safety_backup: true,
        ..RestoreOptions::default()
    };
    ops::restore_with(&site.stack, &file, &without, &mut Feed::default())
        .expect("restored without a safety backup");
    assert_eq!(site.sql("SELECT v FROM scn_marker"), "in the backup");
    writable(&backups);
    eprintln!("scenario 5 took {:.0?}", t.elapsed());
}
